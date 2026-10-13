//! The per-session driver (ADR 0011, ADR 0004 path B).
//!
//! One driver per session; turns are serial (the core's `&mut` already guarantees that).
//! The driver owns the `AgentSession` and drives `start_turn`/`step` itself — **no caller
//! ever sees an `ActiveRun`**: the type never appears in a public signature and the run
//! lives only inside [`Driver::run_turn`].
//!
//! Durable facts the runtime owns are written through `radian-session`'s
//! [`SessionJournal::append_runtime_event`], interleaved with the core's own journal
//! records on the same `seq` counter. The core's lifecycle/tool/progress events are
//! projected to the host as live events (ADR 0010); transcript changes are **never**
//! reconstructed by diffing the transcript — the driver reads the core's
//! `TranscriptAppended` notifications instead.
//!
//! Extensions (MCP servers) are re-opened when the session is reopened; their in-process
//! state does **not** survive a process restart (plan Phase 2, last point).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use agent_core::{
    Agent, AgentEvent, AgentSession, CancelReason, EventSink, Hooks, IncompleteReason,
    JournalError, JournalRecord, Message, MessageId, Model, RunError, RunOutcome, RunStatus,
    SessionError, SteeringHandle, Step, ToolProvider, TranscriptJournal, TurnOptions,
};
use radian_session::{
    EventData, InboxLane, ReadError, RuntimeEvent, Session, SessionJournal, TurnOutcome, TurnReason,
};

use crate::config::{ConfigQueue, ConfigSlot};
use crate::inbox::{Inbox, InboxEntry, Lane};

/// Why the driver could not continue.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("session: {0}")]
    Session(#[from] SessionError),
    #[error("journal: {0}")]
    Journal(#[from] radian_session::WriteError),
    #[error("journal: {0}")]
    CoreJournal(#[from] JournalError),
    #[error("run: {0}")]
    Run(#[from] RunError),
    #[error("reading the session file: {0}")]
    Read(#[from] ReadError),
    #[error("folding the session file: {0}")]
    Replay(#[from] agent_core::ReplayError),
}

/// What recovery learned from the session file before the session was opened.
struct Recovery {
    history: Vec<Message>,
    /// Every accepted input that is not in the transcript, in enqueue order. Pushed back at
    /// the front of its lane by the caller.
    requeue: Vec<InboxEntry>,
    next_turn: u64,
    next_config_revision: u64,
}

/// The per-session driver.
pub struct Driver<M: Model, P: ToolProvider, H: Hooks> {
    inbox: Arc<Inbox>,
    session: AgentSession<M, P, H>,
    journal: Arc<SessionJournal>,
    live: Arc<dyn EventSink>,
    slot: Arc<ConfigSlot>,
    queue: Arc<ConfigQueue>,
    options: TurnOptions,
    /// A per-turn time limit: each turn's deadline is computed when the turn starts, so it
    /// is fresh rather than an absolute instant fixed at startup.
    turn_timeout: Option<std::time::Duration>,
    next_turn: u64,
    next_config_revision: u64,
    closed: bool,
}

impl<M: Model, P: ToolProvider, H: Hooks> Driver<M, P, H> {
    /// Opens a driver over `journal`, recovering the inbox and transcript from the file
    /// first. `journal` must already hold the single-writer lock.
    pub async fn open(
        agent: &Arc<Agent<M, P, H>>,
        journal: Arc<SessionJournal>,
        inbox: Arc<Inbox>,
        live: Arc<dyn EventSink>,
        options: TurnOptions,
    ) -> Result<Self, DriverError> {
        let recovery = recover(&journal).await?;
        // Requeue before opening: the inputs are promises that must survive the restart.
        inbox.restore(recovery.requeue).await;
        let session = agent
            .open_session_with_journal(
                recovery.history,
                Arc::clone(&journal) as Arc<dyn TranscriptJournal>,
            )
            .await?;
        Ok(Self {
            inbox,
            session,
            journal,
            live,
            slot: ConfigSlot::new(serde_json::Value::Null),
            queue: ConfigQueue::new(),
            options,
            turn_timeout: None,
            next_turn: recovery.next_turn,
            next_config_revision: recovery.next_config_revision,
            closed: false,
        })
    }

    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// The applied configuration. A model adapter reads this; the driver writes it only at
    /// a turn boundary.
    pub fn config_slot(&self) -> &Arc<ConfigSlot> {
        &self.slot
    }

    /// Queue a configuration change. It takes effect at the next turn boundary.
    pub fn config_queue(&self) -> &Arc<ConfigQueue> {
        &self.queue
    }

    pub fn session_id(&self) -> &str {
        self.journal.session_id()
    }

    /// The context budget's last computation for this session (ADR 0015 C1), for a status
    /// bar: `Disabled` means no number is claimed (no window configured yet), otherwise the
    /// report's `source` says provider-reported vs. estimated.
    pub fn context_budget_status(&self) -> &agent_core::BudgetStatus {
        self.session.context_budget_status()
    }

    /// Sets a per-turn time limit. Each turn gets a deadline computed at its start, so this
    /// applies to every turn rather than only the first.
    pub fn set_turn_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.turn_timeout = timeout;
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Stops the driver after the current turn and closes the session.
    pub fn shutdown(&mut self) {
        self.closed = true;
        self.inbox.close();
        self.inbox.wake_now();
    }

    /// The driver loop: apply pending configuration at each turn boundary, claim the next
    /// `next_turn` input, run one turn, repeat. Returns when [`Driver::shutdown`] or
    /// `Inbox::close` is called (or after an error).
    ///
    /// The session is closed on **both** paths, so a host that exits after an error still
    /// releases the tool session (for MCP, the servers shut down gracefully instead of being
    /// dropped). A forced exit that aborts the task drops the driver instead, which kills
    /// the servers' process groups — that is the supported use of an abort.
    pub async fn run(&mut self) -> Result<(), DriverError> {
        let result = self.run_loop().await;
        let closed = self.session.close().await;
        match (result, closed) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(DriverError::Session(SessionError::Tools(error))),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn run_loop(&mut self) -> Result<(), DriverError> {
        while !self.closed && !self.inbox.is_closed() {
            let Some(entry) = self.inbox.claim_next_turn().await else {
                // Idle. A `followup`/`steer`/`cancel`/`close` wakes us; `inject` does not.
                self.inbox.wake_signal().await;
                continue;
            };
            self.run_turn(entry).await?;
        }
        Ok(())
    }

    /// Runs exactly one turn for a claimed input. The `ActiveRun` is created and dropped
    /// here; nothing about it escapes.
    async fn run_turn(&mut self, entry: InboxEntry) -> Result<(), DriverError> {
        let turn_id = format!("t-{}", self.next_turn);
        self.next_turn += 1;

        // A new cancellation episode: the next `cancel` is a first cancel again.
        self.inbox.begin_turn();

        // Configuration is applied here, at the turn boundary — never mid-turn (ADR 0004).
        self.apply_pending_config().await?;

        // Clone the handles the turn needs so no `&self` borrow is held while the run
        // borrows the session.
        let journal = Arc::clone(&self.journal);
        let inbox = Arc::clone(&self.inbox);
        let live: Arc<dyn EventSink> = Arc::clone(&self.live);
        let mut options = self.options.clone();
        if let Some(timeout) = self.turn_timeout {
            options.deadline = Some(tokio::time::Instant::now() + timeout);
        }

        journal
            .append_runtime_event(RuntimeEvent::InputClaimed {
                message_ids: vec![entry.id.clone()],
                turn_id: turn_id.clone(),
            })
            .await?;
        journal
            .append_runtime_event(RuntimeEvent::TurnStarted {
                turn_id: turn_id.clone(),
            })
            .await?;

        let mut claimed: Vec<MessageId> = vec![entry.id.clone()];
        let mut admitted: HashSet<MessageId> = HashSet::new();
        let tap = Tap::new(&*live);

        let mut run = self.session.start_turn_with(entry.message, options)?;
        let steering = run.steering_handle();
        let cancellation = run.cancellation_handle();
        // Anything already waiting in the next_step lane joins before the first request.
        drain_steering(&inbox, &journal, &steering, &turn_id, &mut claimed).await?;

        let cancel_generation = inbox.cancel_generation();
        let outcome = loop {
            let mut step = Box::pin(run.step(&tap));
            let result = loop {
                tokio::select! {
                    result = &mut step => break result,
                    _ = inbox.wake_signal() => {
                        if inbox.cancel_generation() != cancel_generation {
                            cancellation.cancel();
                        }
                        drain_steering(&inbox, &journal, &steering, &turn_id, &mut claimed).await?;
                    }
                }
            };
            admit_seen(&journal, &claimed, &mut admitted, &tap).await?;
            match result? {
                Step::Continue => {
                    drain_steering(&inbox, &journal, &steering, &turn_id, &mut claimed).await?;
                }
                Step::Finished(outcome) => break outcome,
            }
        };
        drop(run);
        admit_seen(&journal, &claimed, &mut admitted, &tap).await?;

        // The budget stop's numbers, written beside the outcome so a reader can see why the
        // turn ended and whether the projection was reported or estimated (ADR 0015 C6).
        if let RunStatus::ContextBudget(report) = &outcome.status {
            journal
                .append_runtime_event(RuntimeEvent::ContextBudgetReached {
                    turn_id: turn_id.clone(),
                    source: report.source,
                    projected_tokens: report.projected_tokens,
                    window_tokens: report.window_tokens,
                    threshold_percent: report.threshold_percent,
                    baseline_input_tokens: report.baseline_input_tokens,
                    estimated_growth_tokens: report.estimated_growth_tokens,
                })
                .await?;
        }

        let (outcome, reason, stop_reason) = wire_outcome(&outcome);
        journal
            .append_runtime_event(RuntimeEvent::TurnFinished {
                turn_id,
                outcome,
                reason,
                stop_reason,
            })
            .await?;
        Ok(())
    }

    async fn apply_pending_config(&mut self) -> Result<(), DriverError> {
        for change in self.queue.drain() {
            let revision = self.next_config_revision;
            self.next_config_revision += 1;
            self.journal
                .append_runtime_event(RuntimeEvent::ConfigChanged {
                    revision,
                    change: change.clone(),
                })
                .await?;
            self.slot.apply(change, revision);
        }
        Ok(())
    }
}

/// Claims every waiting `next_step` input, records the claim, and hands it to the core's
/// steering handle at its next safe point.
async fn drain_steering(
    inbox: &Inbox,
    journal: &SessionJournal,
    steering: &SteeringHandle,
    turn_id: &str,
    claimed: &mut Vec<MessageId>,
) -> Result<(), DriverError> {
    let mut batch = Vec::new();
    while let Some(entry) = inbox.claim_next_step().await {
        batch.push(entry);
    }
    if batch.is_empty() {
        return Ok(());
    }
    let ids: Vec<MessageId> = batch.iter().map(|e| e.id.clone()).collect();
    journal
        .append_runtime_event(RuntimeEvent::InputClaimed {
            message_ids: ids,
            turn_id: turn_id.to_owned(),
        })
        .await?;
    for entry in batch {
        let InboxEntry {
            id,
            lane,
            source,
            message,
            seq,
        } = entry;
        match steering.steer(message) {
            Ok(()) => claimed.push(id),
            Err(agent_core::SteerError::RunFinished(message)) => {
                // The turn is winding down: put it back so it is not lost. The claim above
                // stays truthful until recovery requeues it (ADR 0011 L3-2).
                inbox
                    .requeue_front(InboxEntry {
                        id,
                        lane,
                        source,
                        message,
                        seq,
                    })
                    .await;
            }
            Err(agent_core::SteerError::NotUserMessage(_)) => {
                // The inbox only accepts user messages, so this cannot happen.
            }
        }
    }
    Ok(())
}

/// Writes `InputAdmitted` for every claimed id the transcript has actually taken.
async fn admit_seen(
    journal: &SessionJournal,
    claimed: &[MessageId],
    admitted: &mut HashSet<MessageId>,
    tap: &Tap<'_>,
) -> Result<(), DriverError> {
    for id in claimed {
        if admitted.contains(id) {
            continue;
        }
        if let Some(index) = tap.index_of(id) {
            journal
                .append_runtime_event(RuntimeEvent::InputAdmitted {
                    message_id: id.clone(),
                    transcript_index: index,
                })
                .await?;
            // Record it, or the next call would write the same admission again.
            admitted.insert(id.clone());
        }
    }
    Ok(())
}

/// Records the transcript indexes the core reports, so the driver can write
/// `InputAdmitted` without ever diffing the transcript.
struct Tap<'a> {
    host: &'a dyn EventSink,
    appended: Mutex<Vec<(usize, Option<MessageId>)>>,
}

impl<'a> Tap<'a> {
    fn new(host: &'a dyn EventSink) -> Self {
        Self {
            host,
            appended: Mutex::new(Vec::new()),
        }
    }

    fn index_of(&self, id: &MessageId) -> Option<usize> {
        self.appended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find_map(|(index, seen)| (seen.as_ref() == Some(id)).then_some(*index))
    }
}

impl EventSink for Tap<'_> {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::TranscriptAppended { index, message } = &event {
            self.appended
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((*index, message_meta_id(message)));
        }
        self.host.emit(event);
    }
}

fn message_meta_id(message: &Message) -> Option<MessageId> {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id.clone(),
    }
}

/// Maps the core's `RunStatus` to the ADR 0006 §5.6 wire shape.
fn wire_outcome(outcome: &RunOutcome) -> (TurnOutcome, Option<TurnReason>, Option<String>) {
    match &outcome.status {
        RunStatus::Completed => (TurnOutcome::Completed, None, None),
        RunStatus::Refused => (TurnOutcome::Refused, None, None),
        RunStatus::BudgetExceeded => (TurnOutcome::BudgetExceeded, None, None),
        // The numbers (source, projection, window) ride on a sibling ContextBudgetReached
        // event in `run_turn`; the outcome alone is the distinguishable reason.
        RunStatus::ContextBudget(_) => (TurnOutcome::ContextBudget, None, None),
        RunStatus::Failed => (TurnOutcome::Failed, None, None),
        RunStatus::Poisoned => (TurnOutcome::Poisoned, None, None),
        RunStatus::Incomplete(reason) => match reason {
            IncompleteReason::MaxTokens => {
                (TurnOutcome::Incomplete, Some(TurnReason::MaxTokens), None)
            }
            IncompleteReason::ContentFiltered => (
                TurnOutcome::Incomplete,
                Some(TurnReason::ContentFiltered),
                None,
            ),
            IncompleteReason::ToolUseWithoutCalls => (
                TurnOutcome::Incomplete,
                Some(TurnReason::ToolUseWithoutCalls),
                None,
            ),
            IncompleteReason::UnknownStopReason(raw) => (
                TurnOutcome::Incomplete,
                Some(TurnReason::UnknownStopReason),
                Some(raw.clone()),
            ),
        },
        RunStatus::Cancelled(reason) => match reason {
            CancelReason::Requested => (TurnOutcome::Cancelled, Some(TurnReason::Requested), None),
            CancelReason::DeadlineExceeded => (
                TurnOutcome::Cancelled,
                Some(TurnReason::DeadlineExceeded),
                None,
            ),
        },
        // `RunStatus` is `#[non_exhaustive]`: a future terminal reason maps to `Unknown`
        // rather than breaking this build. The same commit that adds a Core variant must
        // give it a real mapping here (ADR 0006 §7(d)).
        _ => (TurnOutcome::Unknown, None, None),
    }
}

fn lane_of(lane: InboxLane) -> Lane {
    match lane {
        InboxLane::NextTurn => Lane::NextTurn,
        InboxLane::NextStep => Lane::NextStep,
        // A lane this version does not know (a newer writer). Requeue as a next-turn input:
        // dropping it would lose the user's task, and `NextTurn` is the safest lane for a
        // v1 reader that cannot know the future semantics (ADR 0006 §7(c)/(d)).
        InboxLane::Unknown => Lane::NextTurn,
    }
}

/// Reads the session file and rebuilds what the driver must not lose (ADR 0011 L3):
///
/// 1. admitted inputs stay admitted (they are in the transcript);
/// 2. every other accepted input goes back to the front of its original lane, in enqueue
///    order — this covers "claimed but not admitted" (L3-2) and, because the inbox is
///    durable, inputs that were enqueued but never claimed before the restart;
/// 3. unpaired tool calls go through the core's `repair` (ADR 0002), and the repaired
///    messages are written back before the session is reopened (ADR 0002 E9).
async fn recover(journal: &SessionJournal) -> Result<Recovery, DriverError> {
    let parsed = Session::open(journal.path())?;
    let records = parsed.journal_records();
    let replayed = agent_core::replay(&records)?;
    let history = agent_core::recover(&records)?;

    // E9: the repaired tail must be durable before the session is opened with it.
    for (offset, message) in history[replayed.len()..].iter().enumerate() {
        journal
            .append(JournalRecord::Message {
                index: replayed.len() + offset,
                message: message.clone(),
            })
            .await?;
    }

    // The transcript is the authority on whether an input was admitted: the input is
    // committed at the **start** of a step, while `InputAdmitted` is written after the step
    // returns. A crash in between leaves the message in the transcript without the record —
    // requeueing it would replay it (ADR 0011 L3-1: "already in the transcript, do not
    // replay"). Its `meta.id` is the identity link (ADR 0006 §5.5).
    let mut in_transcript: HashMap<MessageId, usize> = HashMap::new();
    for (index, message) in replayed.iter().enumerate() {
        if let Some(id) = message_meta_id(message) {
            in_transcript.entry(id).or_insert(index);
        }
    }

    let mut enqueued: Vec<(MessageId, InboxLane, Message, u64)> = Vec::new();
    let mut admitted: HashSet<MessageId> = HashSet::new();
    let mut recorded_admitted: HashSet<MessageId> = HashSet::new();
    let mut next_turn = 1u64;
    // ADR 0006 §5.7: the session-local config revision starts at 1.
    let mut next_config_revision = 1u64;
    for event in parsed.events() {
        match &event.data {
            EventData::InboxEnqueued {
                message,
                message_id,
                lane,
                ..
            } => enqueued.push((message_id.clone(), *lane, message.clone(), event.seq)),
            EventData::InputAdmitted { message_id, .. } => {
                recorded_admitted.insert(message_id.clone());
                admitted.insert(message_id.clone());
            }
            EventData::TurnStarted { turn_id } | EventData::TurnFinished { turn_id, .. } => {
                next_turn = next_turn.max(turn_number(turn_id) + 1);
            }
            EventData::ConfigChanged { revision, .. } => {
                next_config_revision = next_config_revision.max(revision + 1);
            }
            _ => {}
        }
    }

    // Catch the admission record up for anything the transcript already holds, so the
    // journal and the transcript agree before the session is reopened.
    for (id, _, _, _) in &enqueued {
        if recorded_admitted.contains(id) {
            continue;
        }
        if let Some(index) = in_transcript.get(id) {
            journal
                .append_runtime_event(RuntimeEvent::InputAdmitted {
                    message_id: id.clone(),
                    transcript_index: *index,
                })
                .await?;
            admitted.insert(id.clone());
        }
    }

    // Front-most last, so a single front-push per entry restores the enqueue order.
    let mut requeue = Vec::new();
    for (id, lane, message, seq) in enqueued.into_iter().rev() {
        if !admitted.contains(&id) {
            requeue.push(InboxEntry {
                id,
                lane: lane_of(lane),
                source: source_of(&message),
                message,
                seq,
            });
        }
    }
    Ok(Recovery {
        history,
        requeue,
        next_turn,
        next_config_revision,
    })
}

/// The source is recorded on the message when it is enqueued; a message without one never
/// came through the durable inbox. `System` is the safe fallback: it is never `User`, so it
/// cannot be mistaken for human input (ADR 0011 L2).
fn source_of(message: &Message) -> agent_core::InputSource {
    let meta = match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta,
    };
    meta.source.unwrap_or(agent_core::InputSource::System)
}

fn turn_number(turn_id: &str) -> u64 {
    turn_id
        .strip_prefix("t-")
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}
