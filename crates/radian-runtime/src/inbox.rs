//! The durable inbox: the four semantic input operations of ADR 0011 (plus the
//! pending-wake job notice).
//!
//! This is the queue half of the runtime. It holds inputs in two lanes and is the only
//! public way to add work:
//!
//! - [`Inbox::followup`] — `next_turn`: a new user/system task for after the current turn.
//! - [`Inbox::steer`] — `next_step`: change the current turn's direction as soon as the
//!   core's next safe point allows.
//! - [`Inbox::inject`] — `next_step`: context that waits for a natural step boundary and
//!   does **not** wake an idle driver.
//! - [`Inbox::inject_pending_wake`] — `next_step`: a job completion notice that waits for
//!   a natural step boundary while a turn runs, and opens the next turn itself when it is
//!   still queued at the turn boundary (ADR 0011).
//! - [`Inbox::cancel`] — no lane: cancels in-flight model/tool work, is never queued, is
//!   not subject to the capacity bound, and is idempotent.
//!
//! - [`Inbox::compact`] — no lane: a manual `/compact` control request (ADR 0018). It is not
//!   a conversation input: nothing is enqueued and no transcript message is written.
//!
//! There is deliberately no generic splice/insert API (plan §3.3, ADR 0011 L1).
//!
//! # The idle rule (ADR 0011)
//!
//! When no turn is running, the driver opens the next turn with the oldest `next_turn`
//! entry; when that lane is empty, it opens one with the **oldest waking** `next_step` entry
//! (a `steer`); when that lane has no waking entry either, it opens one with the oldest
//! **pending-wake** `next_step` entry (a stranded job notice, ADR 0011). An
//! `inject` never opens a turn by itself: it waits for a natural step boundary inside a turn
//! someone else started. [`InboxEntry::wake`] and [`InboxEntry::pending_wake`] carry that
//! difference, and both are durable, so a resume restores them.
//!
//! # Bounded input (ADR 0011 L4, decision 1)
//!
//! The queue is bounded. When it is full, `followup`/`steer`/`inject` **refuse** the input
//! and return [`InboxError::Full`] **carrying the rejected message**, so the host can keep
//! it and retry. Nothing is ever silently dropped. `cancel` is exempt (decision 3).
//!
//! # Source is required at this boundary
//!
//! ADR 0011  requires `followup`/`steer`/`inject` to take an
//! explicit [`InputSource`]: the core keeps `source: Option` (ADR 0005 G2, `None` means "the
//! host did not say"), and the runtime is where "you must say" is enforced. The inbox writes
//! the source into the message's `meta.source` and into `InboxEnqueued.source`, so the
//! permission layer can always find it.
//!
//! # Durability
//!
//! An inbox built with [`Inbox::durable`] writes `InboxEnqueued` through the session journal
//! **before** the input is visible in memory: an accepted input is a durable promise (ADR
//! 0011 L3). [`Inbox::new`] is the in-memory variant used by the queue tests.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use radian_core::{CompactionOutcome, ContextBudget, InputSource, Message, MessageId};
use radian_session::{InboxLane, RuntimeEvent, SessionJournal};
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

use super::driver::RuntimeEvents;

/// Which queue an input waits in (ADR 0011 L1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Taken at a turn boundary and passed to `start_turn_with` (`followup`).
    NextTurn,
    /// Handed to the core's `SteeringHandle` at its next safe point (`steer`/`inject`).
    NextStep,
}

impl Lane {
    pub(crate) fn wire(self) -> InboxLane {
        match self {
            Lane::NextTurn => InboxLane::NextTurn,
            Lane::NextStep => InboxLane::NextStep,
        }
    }
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
    #[error("the enqueue could not be made durable, so it was not accepted: {error}")]
    Journal { message: Message, error: String },
}

impl InboxError {
    /// The message the caller must keep and retry with.
    pub fn message(&self) -> &Message {
        match self {
            InboxError::Full { message, .. }
            | InboxError::Closed { message }
            | InboxError::NotUser { message }
            | InboxError::Journal { message, .. } => message,
        }
    }

    /// Consumes the error and returns the message.
    pub fn into_message(self) -> Message {
        match self {
            InboxError::Full { message, .. }
            | InboxError::Closed { message }
            | InboxError::NotUser { message }
            | InboxError::Journal { message, .. } => message,
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
    /// Whether this entry wakes an idle driver and may open the next turn itself (ADR 0011
    /// ): a `steer` does, an `inject` does not. `followup` entries wake too, but
    /// they are taken by lane priority before this flag is consulted.
    pub wake: bool,
    /// Whether this `next_step` entry opens the next turn when it is still queued at the
    /// turn boundary (ADR 0011): a stranded job completion notice does, an
    /// ordinary `inject` does not. Absorbed mid-turn exactly like an `inject`.
    pub pending_wake: bool,
    /// The source the caller declared at enqueue time (ADR 0011).
    pub source: InputSource,
    /// The message, with its `meta.id` and `meta.source` set.
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

    fn push(&mut self, entry: InboxEntry) {
        match entry.lane {
            Lane::NextTurn => self.next_turn.push_back(entry),
            Lane::NextStep => self.next_step.push_back(entry),
        }
    }
}

/// Fired with the entry id each time a `pending_wake` entry is claimed to open the
/// next turn (ADR 0011). The host installs its wake-gate counter here.
pub type PendingWakeCallback = Arc<dyn Fn(MessageId) + Send + Sync>;

/// Fired with every entry [`Inbox::claim_next_turn`] takes to open a turn. The host
/// attributes the turn to the input that opened it here (a `Stop` continuation's
/// `stop_hook_active`, ADR 0045 §2): the provenance travels with the queued message,
/// not with a session-global flag. Runs without the queue lock held.
pub type TurnClaimedCallback = Arc<dyn Fn(&InboxEntry) + Send + Sync>;

/// The bounded, two-lane input queue. Share it with `Arc`: the host enqueues, the driver
/// claims.
pub struct Inbox {
    capacity: usize,
    state: Mutex<State>,
    /// Woken by `followup`, `steer`, `inject_pending_wake` and `cancel`; **not** by a
    /// plain `inject`.
    wake: Notify,
    /// How many times the queue changed (every enqueue, claim and requeue). A host that
    /// checks "is there a turn to run" reads it before and after the check: equal means
    /// no entry arrived or left in between, so the check saw a consistent queue. In
    /// particular the driver's claim (which pops silently, with no notification) cannot
    /// slip between the two reads unnoticed.
    version: AtomicU64,
    /// Turn-opening claims outstanding: incremented by [`Inbox::claim_next_turn`] in the
    /// same critical section as the pop, decremented when the turn ends (or the entry
    /// is handed back). A host that decides "may I exit" reads this next to the queue:
    /// a claimed-but-unstarted turn is already visible here, before the driver's
    /// `turn_running` flag is set and before `TurnStarted` reaches any listener — the
    /// window between the pop and those later signals cannot hide a turn from the
    /// check. Only turn-opening claims count (`claim_next_step` runs mid-turn, when the
    /// count is already nonzero).
    inflight: AtomicU64,
    next_seq: AtomicU64,
    next_id: AtomicU64,
    /// A per-process prefix so ids never collide across a restart: the numeric counter
    /// alone would restart at 1 in a new process and a resumed session would see two
    /// different inputs with the same id (ADR 0006 §5.5 keys recovery on `message_id`).
    id_prefix: String,
    /// Number of `cancel` calls (all of them), for observability.
    cancel_calls: AtomicU64,
    /// Increments on the first `cancel` of the current episode; a repeated `cancel` before
    /// the driver begins the next turn changes nothing (decision 3).
    cancel_generation: AtomicU64,
    cancelled: AtomicBool,
    last_cause: std::sync::Mutex<Option<CancelCause>>,
    closed: AtomicBool,
    /// When present, `InboxEnqueued` is written before the input becomes visible.
    journal: Option<Arc<SessionJournal>>,
    /// When present, every accepted `InboxEnqueued` is also forwarded here (after the
    /// journal write, without holding the queue lock): the host's live listener hears the
    /// enqueue without reading the journal. Installed by the driver from its own
    /// `set_runtime_events`, so a host sets one listener, not two.
    runtime_events: std::sync::Mutex<Option<Arc<dyn RuntimeEvents>>>,
    /// Fired (once per entry, without holding the queue lock) when a `pending_wake`
    /// entry is claimed to open the next turn (ADR 0011). The host counts its
    /// wake-gate wake here: an entry absorbed mid-turn never fires, so only a turn the
    /// notice actually opened counts.
    pending_wake_callback: std::sync::Mutex<Option<PendingWakeCallback>>,
    /// Fired (once per entry, without holding the queue lock) with every entry
    /// [`Inbox::claim_next_turn`] takes. The host reads the claimed message's own
    /// provenance here (ADR 0045 §2).
    turn_claimed_callback: std::sync::Mutex<Option<TurnClaimedCallback>>,
    /// Control requests the driver runs between turns (ADR 0018: `/compact`). Not a lane.
    control: Mutex<VecDeque<ControlRequest>>,
}

/// A control request the driver runs between turns (ADR 0018: a manual `/compact`). It is not
/// a conversation input, so it writes no `InboxEnqueued` and no transcript message. `reply`
/// carries the outcome back to the caller (a `String` failure keeps this crate decoupled from
/// the core's error type).
pub struct ControlRequest {
    pub instructions: Option<String>,
    /// The budget this compaction targets (ADR 0026 D7): a `/model` confirm compaction must
    /// reach the **new** model's B', which the configured budget knows nothing about.
    pub budget: Option<ContextBudget>,
    pub reply: tokio::sync::oneshot::Sender<Result<CompactionOutcome, String>>,
}

// Carrying the rejected message in the error is the decision 1 (ADR 0011 L4): the
// caller must get the message back to retry, never just an error code. Boxing it only to
// satisfy a size heuristic would make that contract less direct, exactly as in
// `radian_core::SteerError::RunFinished(Message)`.
#[allow(clippy::result_large_err)]
impl Inbox {
    /// An in-memory inbox (no durability). Used by the queue semantics tests and by hosts
    /// that manage durability themselves.
    pub fn new(capacity: usize) -> Arc<Self> {
        Self::build(capacity, None)
    }

    /// The durable inbox: every accepted input is written as `InboxEnqueued` before it is
    /// visible in memory.
    pub fn durable(capacity: usize, journal: Arc<SessionJournal>) -> Arc<Self> {
        Self::build(capacity, Some(journal))
    }

    fn build(capacity: usize, journal: Option<Arc<SessionJournal>>) -> Arc<Self> {
        assert!(
            capacity >= 1,
            "an inbox with capacity 0 could never accept input"
        );
        Arc::new(Self {
            capacity,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            next_seq: AtomicU64::new(0),
            version: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            next_id: AtomicU64::new(0),
            id_prefix: format!("inbox-{}-", id_nonce()),
            cancel_calls: AtomicU64::new(0),
            cancel_generation: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            last_cause: std::sync::Mutex::new(None),
            closed: AtomicBool::new(false),
            journal,
            runtime_events: std::sync::Mutex::new(None),
            pending_wake_callback: std::sync::Mutex::new(None),
            turn_claimed_callback: std::sync::Mutex::new(None),
            control: Mutex::new(VecDeque::new()),
        })
    }

    /// Installs the host listener for `InboxEnqueued` (the driver calls this from its own
    /// `set_runtime_events`, so a host sets one listener, not two). `None` keeps the
    /// behavior a host that never installed one had before.
    pub fn set_runtime_events(&self, sink: Arc<dyn RuntimeEvents>) {
        *self
            .runtime_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    /// Fires `callback` with the entry id each time a `pending_wake` entry is claimed to
    /// open the next turn (ADR 0011). The host installs its wake-gate counter
    /// here. `None` keeps the behavior a host that never installed one had before.
    pub fn set_pending_wake_callback(&self, callback: Option<PendingWakeCallback>) {
        *self
            .pending_wake_callback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = callback;
    }

    /// Fires `callback` with each entry [`Inbox::claim_next_turn`] takes, so a host can
    /// attribute the turn to the input that opened it (a `Stop` continuation carries its
    /// provenance here, ADR 0045 §2). `None` keeps the behavior a host that never
    /// installed one had before.
    pub fn set_turn_claimed_callback(&self, callback: Option<TurnClaimedCallback>) {
        *self
            .turn_claimed_callback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = callback;
    }

    /// `next_turn`: a new task for after the current turn. Wakes an idle driver.
    pub async fn followup(
        &self,
        input: impl Into<Message>,
        source: InputSource,
    ) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextTurn, input.into(), source, true, false, false)
            .await
    }

    /// `next_step`: change the current turn's direction at the next safe point. Wakes an
    /// idle driver.
    pub async fn steer(
        &self,
        input: impl Into<Message>,
        source: InputSource,
    ) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), source, true, false, false)
            .await
    }

    /// `next_step`: context that waits for a natural step boundary. Does **not** wake an
    /// idle driver.
    pub async fn inject(
        &self,
        input: impl Into<Message>,
        source: InputSource,
    ) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), source, false, false, false)
            .await
    }

    /// `next_step` like [`Inbox::inject`], but a job completion notice that opens the next
    /// turn when it is still queued at the turn boundary (ADR 0011). Absorbed
    /// mid-turn exactly like an `inject`; stranded past the turn end it is claimed by
    /// [`Inbox::claim_next_turn`] instead of waiting for another input to wake the driver.
    ///
    /// Like `steer` (and unlike `inject`) it notifies the driver, so an enqueue that lands
    /// after the driver's last claim still wakes the loop to look: the wake branch only
    /// checks cancellation, so the entry still waits for the turn boundary.
    pub async fn inject_pending_wake(
        &self,
        input: impl Into<Message>,
        source: InputSource,
    ) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), source, false, false, true)
            .await
    }

    /// `next_step` like [`Inbox::inject`], marked as a wake-gate degrade: the wake budget
    /// was used up, so this input waits for a turn without ever opening one itself. The
    /// durable `InboxEnqueued` record says so (`degraded: true`, ADR 0006 §5.5).
    pub async fn inject_degraded(
        &self,
        input: impl Into<Message>,
        source: InputSource,
    ) -> Result<MessageId, InboxError> {
        self.enqueue(Lane::NextStep, input.into(), source, false, true, false)
            .await
    }

    async fn enqueue(
        &self,
        lane: Lane,
        mut message: Message,
        source: InputSource,
        wake: bool,
        degraded: bool,
        pending_wake: bool,
    ) -> Result<MessageId, InboxError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(InboxError::Closed { message });
        }
        if !message.is_user() {
            return Err(InboxError::NotUser { message });
        }
        // The source is explicit at this boundary and is recorded on the message itself, so
        // the transcript carries it too (ADR 0006 §5.5: `meta.source` comes from here).
        set_message_meta_source(&mut message, source);

        // The lock is held across the durable write so that capacity is exact and the
        // in-memory order equals the `seq` order even under concurrent enqueues.
        let mut state = self.state.lock().await;
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
                let id = MessageId(format!("{}{n}", self.id_prefix));
                set_message_meta_id(&mut message, id.clone());
                id
            }
        };
        let event = RuntimeEvent::InboxEnqueued {
            message: message.clone(),
            message_id: id.clone(),
            lane: lane.wire(),
            source,
            // ADR 0011: the entry's waking flag is durable, so a resume
            // knows which `next_step` entries may open a turn themselves. It
            // adds the pending wake the same way.
            wake,
            // A wake-gate degrade (ADR 0006 §5.5): written only when true, so an
            // ordinary line is byte-identical to before.
            degraded,
            // A stranded job notice's pending wake (ADR 0011): written only
            // when true, like `degraded`.
            pending_wake,
        };
        if let Some(journal) = &self.journal {
            // Journal first, then memory (ADR 0001 S1): an input that is not durable was
            // never accepted.
            journal
                .append_runtime_event(event.clone())
                .await
                .map_err(|error| InboxError::Journal {
                    message: message.clone(),
                    error: error.to_string(),
                })?;
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        state.push(InboxEntry {
            id: id.clone(),
            lane,
            wake,
            pending_wake,
            source,
            message,
            seq,
        });
        // Inside the same critical section as the push, so a version read brackets it.
        self.version.fetch_add(1, Ordering::SeqCst);
        drop(state);
        // The host hears the enqueue once it is durable, like the turn boundary: the sink
        // sends on an unbounded channel, so it never blocks, and it runs without the queue
        // lock held.
        if let Some(sink) = self
            .runtime_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            sink.on_runtime_event(&event);
        }
        if wake || pending_wake {
            // A pending wake notifies like a steer: an enqueue that lands after the
            // driver's last claim still wakes the loop to look. The wake branch only
            // checks cancellation, so the entry still waits for the turn boundary.
            self.wake.notify_one();
        }
        Ok(id)
    }

    /// Requests a manual `/compact` (ADR 0018) and waits for its result. The driver runs it
    /// between turns: with a turn in flight this waits for the turn to end. A `cancel`
    /// (Ctrl-C/Esc) aborts it without committing.
    pub async fn compact(&self, instructions: Option<String>) -> Result<CompactionOutcome, String> {
        self.compact_with(instructions, None).await
    }

    /// Like [`Inbox::compact`], with the budget the compaction targets (ADR 0026 D7).
    pub async fn compact_with(
        &self,
        instructions: Option<String>,
        budget: Option<ContextBudget>,
    ) -> Result<CompactionOutcome, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("the session is closed".into());
        }
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.control.lock().await.push_back(ControlRequest {
            instructions,
            budget,
            reply,
        });
        self.wake.notify_one();
        match rx.await {
            Ok(result) => result,
            Err(_) => Err("the driver stopped before the compaction ran".into()),
        }
    }

    /// Removes the oldest control request, if any. Used by the driver between turns.
    pub(crate) async fn claim_control(&self) -> Option<ControlRequest> {
        self.control.lock().await.pop_front()
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

    pub async fn len(&self) -> usize {
        self.state.lock().await.len()
    }

    /// How many times the queue changed. See the field comment.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// A consistent copy of the queue: `next_turn` entries then `next_step` entries, each in
    /// FIFO order. For a UI that displays pending input.
    pub async fn snapshot(&self) -> Vec<InboxEntry> {
        let state = self.state.lock().await;
        state
            .next_turn
            .iter()
            .chain(state.next_step.iter())
            .cloned()
            .collect()
    }

    /// Resolves the next time `followup`/`steer`/`inject_pending_wake`/`cancel` wakes the
    /// driver. A plain `inject` never resolves it.
    ///
    /// Single waiter: the driver. Its `Notify` hands one permit to one waiter, so a
    /// second waiter could steal the wake meant for the driver (or vice versa) and
    /// deadlock the other side: the permit is consumed, the entry stays queued, and no
    /// further signal ever comes. Hosts learn about wakes through `started`, the
    /// settled/stops/idle signals and the queue itself — never by waiting here.
    /// (`radian-runtime`'s and `radian-host`'s integration tests also call this to
    /// observe a wake; that is why it stays `pub`.)
    pub fn wake_signal(&self) -> Notified<'_> {
        self.wake.notified()
    }

    /// Wakes the driver without changing any state (used by shutdown).
    pub(crate) fn wake_now(&self) {
        self.wake.notify_one();
    }

    /// Claims the input that opens the next turn (ADR 0011): the
    /// oldest `next_turn` entry; or, when that lane is empty, the **oldest waking**
    /// `next_step` entry; or, when that lane has no waking entry either, the **oldest
    /// pending-wake** `next_step` entry (a stranded job notice, revision 2). A plain
    /// non-waking `next_step` entry (an `inject`) never opens a turn; it
    /// stays and is absorbed by a turn's first safe point. Every older entry keeps its
    /// relative order: an `inject` pushed before the chosen steer stays in `next_step` in
    /// front of the later entries.
    ///
    /// The turn-claimed callback fires for every claim (the host attributes the turn to
    /// the message); the pending-wake callback fires only when the third branch claims.
    /// Both fire after the queue lock is dropped, like the runtime-events forward: the pop
    /// and the in-flight count still move together.
    ///
    /// The pop and the in-flight count move together, under the queue lock: when this
    /// returns an entry, [`Inbox::inflight_turns`] already counts it. A host checking
    /// "may I exit" between the pop and the turn's start still sees the claimed turn.
    pub async fn claim_next_turn(&self) -> Option<InboxEntry> {
        let entry = {
            let mut state = self.state.lock().await;
            if let Some(entry) = state.next_turn.pop_front() {
                self.version.fetch_add(1, Ordering::SeqCst);
                self.inflight.fetch_add(1, Ordering::SeqCst);
                entry
            } else {
                // `next_step` order: a waking `steer` first, then the oldest
                // pending-wake notice (ADR 0011). A plain `inject` never
                // opens a turn.
                let index = state
                    .next_step
                    .iter()
                    .position(|entry| entry.wake)
                    .or_else(|| state.next_step.iter().position(|entry| entry.pending_wake))?;
                self.version.fetch_add(1, Ordering::SeqCst);
                self.inflight.fetch_add(1, Ordering::SeqCst);
                // `position` just found it, so the remove cannot miss.
                state.next_step.remove(index).expect("claimed entry")
            }
        };
        if let Some(callback) = self
            .turn_claimed_callback
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            callback(&entry);
        }
        if entry.pending_wake
            && let Some(callback) = self
                .pending_wake_callback
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        {
            callback(entry.id.clone());
        }
        Some(entry)
    }

    /// How many turn-opening claims are outstanding (claimed, turn not yet ended).
    /// The driver decrements it when the turn ends, next to clearing `turn_running`.
    pub fn inflight_turns(&self) -> u64 {
        self.inflight.load(Ordering::SeqCst)
    }

    /// Marks one claimed turn finished. The driver calls it when the turn ends, next to
    /// clearing `turn_running`, so a host watching [`Inbox::inflight_turns`] sees the
    /// whole claimed-to-ended span as busy. Every [`Inbox::claim_next_turn`] pop pairs
    /// with exactly one of these (a requeued entry is re-popped, which counts again).
    pub fn turn_ended(&self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
    }

    /// Removes the oldest `next_step` entry, if any. Used by the driver at a safe point.
    pub async fn claim_next_step(&self) -> Option<InboxEntry> {
        let entry = self.state.lock().await.next_step.pop_front();
        if entry.is_some() {
            self.version.fetch_add(1, Ordering::SeqCst);
        }
        entry
    }

    /// Puts a claimed entry back at the front of its lane (recovery, ADR 0011 L3-2).
    pub async fn requeue_front(&self, entry: InboxEntry) {
        let mut state = self.state.lock().await;
        match entry.lane {
            Lane::NextTurn => state.next_turn.push_front(entry),
            Lane::NextStep => state.next_step.push_front(entry),
        }
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// Puts recovered entries back, preserving their relative order. The input is in
    /// reverse order (front-most last), so a single front-push per entry restores it.
    pub async fn restore(&self, entries: Vec<InboxEntry>) {
        for entry in entries {
            self.requeue_front(entry).await;
        }
    }

    /// Starts a new cancellation episode: the next `cancel` counts as a first cancel again.
    pub fn begin_turn(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
        *self.lock_cause() = None;
    }

    /// Rejects all future input and wakes the driver so it can notice and stop. Closing is
    /// how a host shuts a driver down without holding `&mut Driver` (the driver owns itself
    /// on its task).
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn lock_cause(&self) -> std::sync::MutexGuard<'_, Option<CancelCause>> {
        self.last_cause.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A process-wide sequence for `id_nonce`. The wall clock alone is not enough: the clock's
/// granularity can hand two calls in a tight loop the **same** nanosecond (observed on macOS,
/// where two `Inbox::new` in one test collided), and two inboxes in one process would then
/// generate the same prefix - and their counters both start at 1, so the ids collided.
static NONCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A value that differs between processes (and between runs) **and** between calls in one
/// process: the process id, the wall clock in nanoseconds, and a process-wide counter. Ids
/// only need to be unique, not ordered or guessable.
fn id_nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    id_nonce_at(std::process::id(), nanos, seq)
}

/// The nonce's composition, split out so a test can inject the **same** clock reading twice
/// (the collision the counter exists for).
fn id_nonce_at(pid: u32, nanos: u128, seq: u64) -> String {
    format!("{pid:x}{nanos:x}{seq:x}")
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

fn set_message_meta_source(message: &mut Message, source: InputSource) {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.source = Some(source),
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
                inbox
                    .followup(format!("m{i}"), InputSource::User)
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let state = inbox.state.lock().await;
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
        let id = inbox.followup("hi", InputSource::User).await.unwrap();
        let entry = inbox.snapshot().await.pop().unwrap();
        assert_eq!(entry.id, id);
        assert_eq!(message_meta_id(&entry.message), Some(&id));
        assert_eq!(entry.message_meta_source(), Some(InputSource::User));
    }

    #[tokio::test]
    async fn two_inboxes_never_generate_the_same_id() {
        // Across a restart the counter restarts at 1; the per-process prefix is what keeps a
        // resumed session's ids distinct from the previous process's.
        let first = Inbox::new(4);
        let second = Inbox::new(4);
        let a = first.followup("a", InputSource::User).await.unwrap();
        let b = second.followup("b", InputSource::User).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn inboxes_built_in_a_tight_loop_never_generate_the_same_id() {
        // The regression the nonce's counter exists for: a tight loop can read the same
        // nanosecond for many `Inbox::new` calls, and with a clock-only nonce every inbox
        // then started at `...-1`. 512 is enough to cross a clock tick many times over.
        let mut ids = std::collections::HashSet::new();
        for _ in 0..512 {
            let inbox = Inbox::new(4);
            let id = inbox.followup("x", InputSource::User).await.unwrap();
            assert!(ids.insert(id), "an id repeated within one process");
        }
    }

    #[test]
    fn the_nonce_differs_for_the_same_clock_reading() {
        // Injected directly: the same pid **and** the same nanosecond (the collision), which
        // the process-wide counter must still separate.
        assert_ne!(
            id_nonce_at(7, 123_456_789, 0),
            id_nonce_at(7, 123_456_789, 1)
        );
        assert_ne!(
            id_nonce_at(7, 123_456_789, 1),
            id_nonce_at(7, 123_456_789, 2)
        );
        // And the other two parts still differ across processes and runs.
        assert_ne!(
            id_nonce_at(7, 123_456_789, 0),
            id_nonce_at(8, 123_456_789, 0)
        );
        assert_ne!(
            id_nonce_at(7, 123_456_789, 0),
            id_nonce_at(7, 123_456_790, 0)
        );
    }

    #[test]
    fn id_nonce_consumes_the_counter() {
        // The injected case above only pins the composition; this pins the wiring: if
        // `id_nonce` ever stops calling `fetch_add` (a constant seq), two nonces can share a
        // clock reading again and the injected test would not notice.
        let before = NONCE_SEQ.load(Ordering::Relaxed);
        let _ = id_nonce();
        let after = NONCE_SEQ.load(Ordering::Relaxed);
        assert!(after > before, "id_nonce must consume NONCE_SEQ");
    }

    #[tokio::test]
    async fn a_caller_supplied_id_is_kept() {
        let inbox = Inbox::new(4);
        let mut message = Message::user("hi");
        set_message_meta_id(&mut message, MessageId("host-7".into()));
        let id = inbox.followup(message, InputSource::User).await.unwrap();
        assert_eq!(id, MessageId("host-7".into()));
    }

    trait MetaSource {
        fn message_meta_source(&self) -> Option<InputSource>;
    }

    impl MetaSource for InboxEntry {
        fn message_meta_source(&self) -> Option<InputSource> {
            match &self.message {
                Message::User { meta, .. }
                | Message::Assistant { meta, .. }
                | Message::Tool { meta, .. } => meta.source,
            }
        }
    }
}
