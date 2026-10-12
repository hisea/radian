//! The durable inbox: the four semantic input operations of ADR 0011.
//!
//! This is the queue half of the runtime. It holds inputs in two lanes and is the only
//! public way to add work:
//!
//! - [`Inbox::followup`] — `next_turn`: a new user/system task for after the current turn.
//! - [`Inbox::steer`] — `next_step`: change the current turn's direction as soon as the
//!   core's next safe point allows.
//! - [`Inbox::inject`] — `next_step`: context that waits for a natural step boundary and
//!   does **not** wake an idle driver.
//! - [`Inbox::cancel`] — no lane: cancels in-flight model/tool work, is never queued, is
//!   not subject to the capacity bound, and is idempotent.
//!
//! There is deliberately no generic splice/insert API (plan §3.3, ADR 0011 L1).
//!
//! # Bounded input (ADR 0011 L4, decision 1)
//!
//! The queue is bounded. When it is full, `followup`/`steer`/`inject` **refuse** the input
//! and return [`InboxError::Full`] **carrying the rejected message**, so the host can keep
//! it and retry. Nothing is ever silently dropped. `cancel` is exempt.
//!
//! # Source is optional
//!
//! ADR 0011 leaves "must `followup`/`inject` carry a `source`?" undecided. This crate
//! follows the core: the message's [`agent_core::MessageMeta`] may carry a `source`, and
//! the inbox neither requires nor invents one. A `None` source is not `User` (ADR 0005 G2).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use agent_core::{Message, MessageId};
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// Which queue an input waits in (ADR 0011 L1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Taken at a turn boundary and passed to `start_turn_with` (`followup`).
    NextTurn,
    /// Handed to the core's `SteeringHandle` at its next safe point (`steer`/`inject`).
    NextStep,
}

/// Why an input was refused. Every variant carries the rejected message back (ADR 0011
/// decision 1: never drop silently, hand the message to the caller).
#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("the inbox is full ({capacity} inputs); the message was not accepted")]
    Full { message: Message, capacity: usize },
    #[error("the inbox is closed; the message was not accepted")]
    Closed { message: Message },
    #[error("only user messages can be enqueued in v1")]
    NotUser { message: Message },
}

impl InboxError {
    /// The message the caller must keep and retry with.
    pub fn message(&self) -> &Message {
        match self {
            InboxError::Full { message, .. }
            | InboxError::Closed { message }
            | InboxError::NotUser { message } => message,
        }
    }

    /// Consumes the error and returns the message.
    pub fn into_message(self) -> Message {
        match self {
            InboxError::Full { message, .. }
            | InboxError::Closed { message }
            | InboxError::NotUser { message } => message,
        }
    }
}

/// Why a cancellation was requested. It is recorded so a driver can describe the turn it
/// ended; the queue itself does not interpret it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelCause {
    Requested,
    DeadlineExceeded,
    Other(String),
}

/// One queued input, in enqueue order.
#[derive(Debug, Clone)]
pub struct InboxEntry {
    pub id: MessageId,
    pub lane: Lane,
    /// The message, with its `meta.id` set to `id`.
    pub message: Message,
    /// Monotonic enqueue order within this inbox. Entries in a lane are FIFO by this value.
    pub seq: u64,
}

#[derive(Default)]
struct State {
    next_turn: VecDeque<InboxEntry>,
    next_step: VecDeque<InboxEntry>,
}

impl State {
    fn len(&self) -> usize {
        self.next_turn.len() + self.next_step.len()
    }
}

/// The bounded, two-lane input queue. Share it with `Arc`: the host enqueues, the driver
/// claims.
pub struct Inbox {
    capacity: usize,
    state: std::sync::Mutex<State>,
    /// Woken by `followup`, `steer` and `cancel`; **not** by `inject`.
    wake: Notify,
    next_seq: AtomicU64,
    next_id: AtomicU64,
    /// Number of `cancel` calls (all of them), for observability.
    cancel_calls: AtomicU64,
    /// Increments on the first `cancel` of the current episode; a repeated `cancel` before
    /// the driver begins the next turn changes nothing.
    cancel_generation: AtomicU64,
    cancelled: AtomicBool,
    last_cause: std::sync::Mutex<Option<CancelCause>>,
    closed: AtomicBool,
}

// Carrying the rejected message in the error is the decision 1 (ADR 0011 L4): the
// caller must get the message back to retry, never just an error code. Boxing it only to
// satisfy a size heuristic would make that contract less direct, exactly as in
// `agent_core::SteerError::RunFinished(Message)`.
#[allow(clippy::result_large_err)]
impl Inbox {
    /// Creates an inbox that accepts at most `capacity` queued inputs (both lanes
    /// together). `capacity` must be at least 1.
    pub fn new(capacity: usize) -> Arc<Self> {
        assert!(
            capacity >= 1,
            "an inbox with capacity 0 could never accept input"
        );
        Arc::new(Self {
            capacity,
            state: std::sync::Mutex::new(State::default()),
            wake: Notify::new(),
            next_seq: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
            cancel_calls: AtomicU64::new(0),
            cancel_generation: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            last_cause: std::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }

    /// `next_turn`: a new task for after the current turn. Wakes an idle driver.
    pub async fn followup(&self, input: impl Into<Message>) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextTurn, input.into(), true)
    }

    /// `next_step`: change the current turn's direction at the next safe point. Wakes an
    /// idle driver.
    pub async fn steer(&self, input: impl Into<Message>) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), true)
    }

    /// `next_step`: context that waits for a natural step boundary. Does **not** wake an
    /// idle driver.
    pub async fn inject(&self, input: impl Into<Message>) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), false)
    }

    fn enqueue(
        &self,
        lane: Lane,
        mut message: Message,
        wake: bool,
    ) -> Result<MessageId, InboxError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(InboxError::Closed { message });
        }
        if !message.is_user() {
            return Err(InboxError::NotUser { message });
        }
        let mut state = self.lock();
        if state.len() >= self.capacity {
            return Err(InboxError::Full {
                message,
                capacity: self.capacity,
            });
        }
        let id = match message_meta_id(&message) {
            Some(existing) => existing.clone(),
            None => {
                let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
                let id = MessageId(format!("inbox-{n}"));
                set_message_meta_id(&mut message, id.clone());
                id
            }
        };
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let entry = InboxEntry {
            id: id.clone(),
            lane,
            message,
            seq,
        };
        match lane {
            Lane::NextTurn => state.next_turn.push_back(entry),
            Lane::NextStep => state.next_step.push_back(entry),
        }
        drop(state);
        if wake {
            self.wake.notify_one();
        }
        Ok(id)
    }

    /// Cancels in-flight work. Never queued, never subject to capacity, idempotent: calling
    /// it again before the next turn begins returns normally and has no second effect.
    pub fn cancel(&self, cause: CancelCause) {
        self.cancel_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .cancelled
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            *self.lock_cause() = Some(cause);
            self.cancel_generation.fetch_add(1, Ordering::SeqCst);
        }
        // Waking is harmless when already cancelled; the driver re-checks the generation.
        self.wake.notify_one();
    }

    /// Increments once per cancellation episode. The driver compares it across its turn to
    /// know whether to cancel the in-flight run.
    pub fn cancel_generation(&self) -> u64 {
        self.cancel_generation.load(Ordering::SeqCst)
    }

    pub fn last_cancel_cause(&self) -> Option<CancelCause> {
        self.lock_cause().clone()
    }

    /// How many `cancel` calls were made, including repeats.
    pub fn cancel_calls(&self) -> u64 {
        self.cancel_calls.load(Ordering::SeqCst)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A consistent copy of the queue: `next_turn` entries then `next_step` entries, each in
    /// FIFO order. For a UI that displays pending input.
    pub fn snapshot(&self) -> Vec<InboxEntry> {
        let state = self.lock();
        state
            .next_turn
            .iter()
            .chain(state.next_step.iter())
            .cloned()
            .collect()
    }

    /// Resolves the next time `followup`/`steer`/`cancel` wakes the driver. `inject` never
    /// resolves it.
    pub fn wake_signal(&self) -> Notified<'_> {
        self.wake.notified()
    }

    /// Removes the oldest `next_turn` entry, if any. Used by the driver at a turn boundary.
    pub fn claim_next_turn(&self) -> Option<InboxEntry> {
        self.lock().next_turn.pop_front()
    }

    /// Removes the oldest `next_step` entry, if any. Used by the driver at a safe point.
    pub fn claim_next_step(&self) -> Option<InboxEntry> {
        self.lock().next_step.pop_front()
    }

    /// Puts a claimed entry back at the front of its lane (recovery, ADR 0011 L3-2).
    pub fn requeue_front(&self, entry: InboxEntry) {
        let mut state = self.lock();
        match entry.lane {
            Lane::NextTurn => state.next_turn.push_front(entry),
            Lane::NextStep => state.next_step.push_front(entry),
        }
    }

    /// Starts a new cancellation episode: the next `cancel` counts as a first cancel again.
    pub fn begin_turn(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        *self.lock_cause() = None;
    }

    /// Rejects all future input. Not used yet; the driver owns session lifetime.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_cause(&self) -> std::sync::MutexGuard<'_, Option<CancelCause>> {
        self.last_cause.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn message_meta_id(message: &Message) -> Option<&MessageId> {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id.as_ref(),
    }
}

fn set_message_meta_id(message: &mut Message, id: MessageId) {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id = Some(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unit test (private access): the lane is FIFO by the monotonic enqueue `seq`,
    /// independent of how concurrently the calls arrive.
    #[tokio::test]
    async fn lane_order_is_fifo_by_enqueue_seq() {
        let inbox = Inbox::new(64);
        let mut tasks = Vec::new();
        for i in 0..32u32 {
            let inbox = Arc::clone(&inbox);
            tasks.push(tokio::spawn(async move {
                inbox.followup(format!("m{i}")).await.unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let state = inbox.lock();
        let seqs: Vec<u64> = state.next_turn.iter().map(|e| e.seq).collect();
        assert_eq!(
            seqs,
            (0..32).collect::<Vec<_>>(),
            "enqueue order must be the queue order"
        );
    }

    #[tokio::test]
    async fn an_assigned_id_is_written_into_the_message_meta() {
        let inbox = Inbox::new(4);
        let id = inbox.followup("hi").await.unwrap();
        let entry = inbox.snapshot().pop().unwrap();
        assert_eq!(entry.id, id);
        assert_eq!(message_meta_id(&entry.message), Some(&id));
    }

    #[tokio::test]
    async fn a_caller_supplied_id_is_kept() {
        let inbox = Inbox::new(4);
        let mut message = Message::user("hi");
        set_message_meta_id(&mut message, MessageId("host-7".into()));
        let id = inbox.followup(message).await.unwrap();
        assert_eq!(id, MessageId("host-7".into()));
    }
}
