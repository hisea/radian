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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{
    Agent, AgentEvent, AgentSession, CancelReason, CancellationToken, CompactionFailure,
    ContextBudget, EventSink, Hooks, IncompleteReason, InputSource, JournalError, JournalRecord,
    Message, MessageId, Model, RunError, RunOutcome, RunStatus, SessionError, SteeringHandle, Step,
    ToolProvider, TranscriptJournal, TurnOptions,
};
use radian_session::{
    EventData, InboxLane, ReadError, RuntimeEvent, Session, SessionJournal, TurnOutcome, TurnReason,
};

use crate::config::{ConfigQueue, ConfigSlot, ModelRoute};

/// Computes `B` for a (provider, model) from the warm local catalog (ADR 0026 D7). The host
/// supplies it; it must not touch the network. It returns the whole [`ContextBudget`], not just
/// the token count, so the host can also say whether the route replays reasoning and the
/// estimate must count it (ADR 0014 Q4).
pub type BudgetFor = Arc<dyn Fn(&str, &str) -> Option<ContextBudget> + Send + Sync>;

/// A host seam that adds context to a **user** turn before its first model call (ADR 0021
/// revision 2 decision 3): the memory addition. It is called with the turn's user text and the
/// transcript **before** the turn's input is committed, and returns the one message the driver
/// puts in the inbox's `inject` lane — or `None`, which is the common case.
///
/// The driver only calls it for an input whose declared source is [`InputSource::User`], and it
/// injects with [`InputSource::System`]: the message is context, never human input (ADR 0005
/// G2). A `None` return or an inbox refusal changes nothing about the turn.
pub type TurnAdditions = Arc<dyn Fn(&str, &[Message]) -> Option<Message> + Send + Sync>;

/// Computes the request-level settings the **compaction** (summary) call carries for a
/// (provider, model) (ADR 0018). The host supplies it; it converts the configured
/// `[compaction] thinking` level to the route's wire value with the same map `/thinking` uses.
/// It must not touch the network. Like [`BudgetFor`], it lets the summary settings follow a
/// `/model` switch: a frozen value would send `reasoning_effort` to an Anthropic route (or the
/// reverse) and the provider would reject the request.
pub type CompactionSettingsFor = Arc<dyn Fn(&str, &str) -> serde_json::Value + Send + Sync>;

use crate::inbox::{ControlRequest, Inbox, InboxEntry, Lane};

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

/// A host callback for the runtime events the driver writes, so a host can react to a
/// `TurnFailed` (ADR 0017) without polling the journal. `agent_core::AgentEvent` is left
/// unchanged: a new variant there would be a Core public-API change, and the failure is a
/// runtime event, not a core one.
///
/// A closure `Fn(&RuntimeEvent)` implements this, so a host installs one with
/// `driver.set_runtime_events(Arc::new(move |event| { ... }))`.
pub trait RuntimeEvents: Send + Sync {
    fn on_runtime_event(&self, event: &RuntimeEvent);

    /// A generic host-side hook event: the control port forwards it to subscribers with the
    /// stable name and a JSON payload. The default is a no-op, so a plain sink (or the closure
    /// impl below) ignores it and only the control fanout has to override it.
    fn on_hook_event(&self, name: &str, data: &serde_json::Value) {
        let _ = (name, data);
    }
}

impl<F> RuntimeEvents for F
where
    F: Fn(&RuntimeEvent) + Send + Sync,
{
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        self(event);
    }
}

/// What recovery learned from the session file before the session was opened.
struct Recovery {
    history: Vec<Message>,
    /// Every accepted input that is not in the transcript, in enqueue order. Pushed back at
    /// the front of its lane by the caller.
    requeue: Vec<InboxEntry>,
    next_turn: u64,
    next_config_revision: u64,
    /// The last provider-reported `input` and the transcript prefix (length) that produced it,
    /// for restoring the context-budget baseline (ADR 0015 C1). `None` means no report in the
    /// current transcript revision, so the projection stays a pure estimate.
    budget_baseline: Option<(usize, u64)>,
    /// The revision of the last committed replacement (ADR 0018), 0 when the file has none,
    /// so the next automatic compaction uses a strictly larger revision.
    transcript_revision: u64,
    /// Every `ConfigChanged` change merged in revision order (ADR 0026 D8/D11), and the last
    /// revision seen. Merging **all** of them — not "the last of each kind" — is what keeps a
    /// thinking change, a model change and an `approval` change (pi/auto) from erasing each
    /// other on resume; a future key needs no new arm here.
    config_state: Option<(serde_json::Value, u64)>,
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
    /// Test hook: how long to wait after the turn ends and before the journaled
    /// `TurnFinished` (a controllable gap between `RunFinished` and the commit).
    commit_delay: Option<std::time::Duration>,
    /// A host listener for the runtime events this driver writes (ADR 0017). `None` keeps the
    /// behavior a host that never installed one had before.
    runtime_events: Option<Arc<dyn RuntimeEvents>>,
    /// Test seam: fires when the turn loop enters the wake branch, so the deadlock test can
    /// prove the branch does not block on the journal without relying on timing.
    #[cfg(feature = "testing")]
    testing_wake_branch: Arc<tokio::sync::Notify>,
    next_turn: u64,
    next_config_revision: u64,
    closed: bool,
    /// The recovered transcript (the same fold `Session::history` produces). Handed to the
    /// host so a resume does not re-open and re-fold the journal (resume-perf B). Taken once.
    recovered_history: Option<Vec<Message>>,
    /// Computes `B` for a (provider, model) from the local catalog (ADR 0026 D7). The host
    /// sets it; without it a switch cannot move the budget.
    budget_for: Option<BudgetFor>,
    /// The budget the next turn runs under: set when a model change is applied, `None` keeps
    /// the configured one.
    turn_budget: Option<ContextBudget>,
    /// Computes the compaction call's request-level settings for a (provider, model) (ADR 0018
    /// revision 8). The host sets it; without it a switch cannot move the settings.
    compaction_settings_for: Option<CompactionSettingsFor>,
    /// The compaction settings the next turn runs under: set when a model change is applied,
    /// `None` keeps the configured value.
    turn_compaction_settings: Option<serde_json::Value>,
    /// `true` while a turn (or a `/compact` control request) is in flight (ADR 0037 decision
    /// 3). The host shares it so a job-settle notifier can choose the `inject` lane (busy) or
    /// the waking `followup` lane (idle).
    turn_running: Arc<AtomicBool>,
    /// The host's per-turn addition seam (ADR 0021). `None` keeps the
    /// behavior a host that never installed one had before.
    turn_additions: Option<TurnAdditions>,
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
        let slot = ConfigSlot::new(serde_json::Value::Null);
        let queue = ConfigQueue::new();
        Self::open_with_slot(agent, journal, inbox, live, options, slot, queue).await
    }

    /// Like [`Driver::open`], with the host's own slot **and queue**: the switchable model
    /// reads the slot instance the driver writes, and `/model` pushes into the queue the
    /// driver drains at the turn boundary (ADR 0026 D5).
    #[allow(clippy::too_many_arguments)]
    pub async fn open_with_slot(
        agent: &Arc<Agent<M, P, H>>,
        journal: Arc<SessionJournal>,
        inbox: Arc<Inbox>,
        live: Arc<dyn EventSink>,
        options: TurnOptions,
        slot: Arc<ConfigSlot>,
        queue: Arc<ConfigQueue>,
    ) -> Result<Self, DriverError> {
        Self::open_with_slot_parsed(agent, journal, inbox, live, options, slot, queue, None).await
    }

    /// [`Driver::open_with_slot`], reusing a `Session` the host already parsed (resume-perf
    /// B). The recovery would otherwise open and parse the whole journal a second time.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_with_slot_parsed(
        agent: &Arc<Agent<M, P, H>>,
        journal: Arc<SessionJournal>,
        inbox: Arc<Inbox>,
        live: Arc<dyn EventSink>,
        options: TurnOptions,
        slot: Arc<ConfigSlot>,
        queue: Arc<ConfigQueue>,
        parsed: Option<&radian_session::Session>,
    ) -> Result<Self, DriverError> {
        let recovery = recover(&journal, parsed).await?;
        // The recovered transcript is the host's resume view too (resume-perf B): keep one
        // copy for it before the original moves into the agent session.
        let recovered_history = recovery.history.clone();
        // Requeue before opening: the inputs are promises that must survive the restart.
        inbox.restore(recovery.requeue).await;
        let mut session = agent
            .open_session_with_journal(
                recovery.history,
                Arc::clone(&journal) as Arc<dyn TranscriptJournal>,
            )
            .await?;
        // ADR 0015 C1: a resume recovers the reported `input` baseline from the journal, so
        // the guard does not restart from a pure estimate. `false` (a shorter history) keeps
        // the estimate rather than inventing a baseline.
        if let Some((prefix_len, input)) = recovery.budget_baseline {
            session.restore_budget_baseline(prefix_len, input);
        }
        // ADR 0018 §B: the next compaction's revision must be strictly larger than the last
        // committed one, so it is restored from the file too.
        session.restore_transcript_revision(recovery.transcript_revision);
        let mut driver = Self {
            inbox,
            session,
            journal,
            live,
            slot,
            queue,
            options,
            turn_timeout: None,
            commit_delay: None,
            runtime_events: None,
            #[cfg(feature = "testing")]
            testing_wake_branch: Arc::new(tokio::sync::Notify::new()),
            next_turn: recovery.next_turn,
            next_config_revision: recovery.next_config_revision,
            recovered_history: Some(recovered_history),
            budget_for: None,
            turn_budget: None,
            compaction_settings_for: None,
            turn_compaction_settings: None,
            turn_running: Arc::new(AtomicBool::new(false)),
            turn_additions: None,
            closed: false,
        };
        // D8: resume restores the last recorded change (the model and its budget) before the
        // first turn, so the switchable model and the guard start where the journal left off.
        if let Some((state, revision)) = recovery.config_state {
            driver.apply_change(state, revision);
        }
        Ok(driver)
    }

    pub fn inbox(&self) -> &Arc<Inbox> {
        &self.inbox
    }

    /// Sets the resolver that recomputes `B` for a switched model (ADR 0026 D7). The host
    /// supplies it; it must not touch the network (the startup fetch warmed the cache).
    /// Runs the turn-boundary apply now (`testing` only): the D7 assertion needs the budget a
    /// real boundary would compute without spawning the run loop.
    #[cfg(feature = "testing")]
    pub async fn test_apply_pending_config(&mut self) -> Result<(), DriverError> {
        self.apply_pending_config().await
    }

    /// Takes the recovered transcript (the resume view). `None` after the first call.
    pub fn take_recovered_history(&mut self) -> Option<Vec<Message>> {
        self.recovered_history.take()
    }

    pub fn set_budget_for(&mut self, resolver: BudgetFor) {
        self.budget_for = Some(resolver);
        // Recompute at once for the route already in the slot (review M2): on a resume the
        // replay happens before the host can set the resolver, so without this the restored
        // model would keep `B = None` until the next switch.
        if let Some(route) = ModelRoute::from_value(&self.slot.value())
            && let Some(resolver) = &self.budget_for
        {
            self.turn_budget = resolver(&route.provider, &route.model);
        }
    }

    /// Sets the resolver that recomputes the compaction call's request-level settings for a
    /// switched model (ADR 0018). Like [`Driver::set_budget_for`], it recomputes for
    /// the route already in the slot at once, so a resume restores the right form before the
    /// first turn.
    pub fn set_compaction_settings_for(&mut self, resolver: CompactionSettingsFor) {
        self.compaction_settings_for = Some(resolver);
        if let Some(route) = ModelRoute::from_value(&self.slot.value())
            && let Some(resolver) = &self.compaction_settings_for
        {
            self.turn_compaction_settings = Some(resolver(&route.provider, &route.model));
        }
    }

    /// The budget the next turn runs under (`testing` only): the D7 assertion reads it instead
    /// of reaching into a running turn.
    #[cfg(feature = "testing")]
    pub fn test_turn_budget(&self) -> Option<ContextBudget> {
        self.turn_budget
    }

    /// The shared "a turn is in flight" flag (ADR 0037 decision 3). The host clones it before
    /// the driver moves into its task; the driver sets it around every turn and control run.
    pub fn turn_running_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.turn_running)
    }

    /// Installs the host's per-turn addition seam (ADR 0021). Set it
    /// before `run()`; the last one wins.
    pub fn set_turn_additions(&mut self, additions: TurnAdditions) {
        self.turn_additions = Some(additions);
    }

    /// Overrides the shared flag (a host that wants its own handle).
    pub fn set_turn_running(&mut self, flag: Arc<AtomicBool>) {
        self.turn_running = flag;
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

    /// Test hook: delay the journaled `TurnFinished` by `delay`, so a test can control the
    /// gap between `RunFinished` (the UI's idle) and the commit.
    pub fn set_commit_delay(&mut self, delay: Option<std::time::Duration>) {
        self.commit_delay = delay;
    }

    /// Test seam: the wake-branch signal (fired when the turn loop enters the wake branch).
    #[cfg(feature = "testing")]
    pub fn testing_wake_branch(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.testing_wake_branch)
    }

    /// Installs the host listener for the runtime events this driver writes (ADR 0017). The
    /// driver calls it for `TurnFailed` so a host can show the error without reading the
    /// journal. Set it before `run()`; the last one wins.
    ///
    /// The sink also hears `InboxEnqueued` (forwarded by the inbox, after the journal
    /// write) and `InputAdmitted` (forwarded here, once per input): a host that shows the
    /// queue — the TUI's `queued` footer item — reads them live instead of polling.
    pub fn set_runtime_events(&mut self, sink: Arc<dyn RuntimeEvents>) {
        self.inbox.set_runtime_events(Arc::clone(&sink));
        self.runtime_events = Some(sink);
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

    /// The driver loop: apply pending configuration at each turn boundary, claim the input
    /// that opens the next turn (`next_turn` first; when that lane is empty, the oldest
    /// waking `next_step` entry — ADR 0011), run one turn, repeat.
    /// Returning to this loop after every turn is decision 3: the check right after
    /// `requeue_unadmitted` needs no wake, and it covers the normal finish and the
    /// model-error finish (ADR 0017) alike. Returns when [`Driver::shutdown`] or
    /// `Inbox::close` is called (or after an error).
    ///
    /// The session is closed on **both** paths, so a host that exits after an error still
    /// releases the tool session (for MCP, the servers shut down gracefully instead of being
    /// dropped). A forced exit that aborts the task drops the driver instead, which kills
    /// the servers' process groups — that is the supported use of an abort.
    pub async fn run(&mut self) -> Result<(), DriverError> {
        let result = self.run_loop().await;
        // A change queued during the final turn (ADR 0026 D9's approval switch is the case:
        // it is made mid-turn, and the host may stop right after) has no next turn boundary
        // to land on. Journal it before the session closes, so a resume still restores it.
        // A queued change that the journal cannot take is a real failure only when nothing
        // else already failed; the original result wins otherwise.
        let result = match self.apply_pending_config().await {
            Ok(()) => result,
            Err(error) => Err(match result {
                Ok(()) => error,
                Err(original) => original,
            }),
        };
        let closed = self.session.close().await;
        match (result, closed) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(DriverError::Session(SessionError::Tools(error))),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn run_loop(&mut self) -> Result<(), DriverError> {
        while !self.closed && !self.inbox.is_closed() {
            // A manual `/compact` runs **between turns** (ADR 0018). A request that arrived
            // during a turn has waited for it to end; the wake branch never runs one.
            if let Some(request) = self.inbox.claim_control().await {
                self.turn_running.store(true, Ordering::SeqCst);
                let result = self.run_control(request).await;
                self.turn_running.store(false, Ordering::SeqCst);
                result?;
                continue;
            }
            let Some(entry) = self.inbox.claim_next_turn().await else {
                // Idle. A `followup`/`steer`/`cancel`/`close` wakes us; `inject` does not.
                // Claiming also opens a turn with a leftover waking `next_step` entry (ADR
                // 0011 revision 1 decision 2), so the wake above is not wasted on an empty
                // `next_turn` lane.
                self.inbox.wake_signal().await;
                continue;
            };
            self.turn_running.store(true, Ordering::SeqCst);
            let result = self.run_turn(entry).await;
            self.turn_running.store(false, Ordering::SeqCst);
            result?;
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
        let runtime_events = self.runtime_events.clone();
        let turn_additions = self.turn_additions.clone();
        let mut options = self.options.clone();
        // ADR 0036: the tool definitions are never withdrawn mid-session.
        // This driver has no step ceiling (the request carries every mounted tool on every
        // call); if a per-agent step limit ever lands (a future step limit), reaching it must
        // keep the definitions and set `tool_choice: none` instead, so the provider's cached
        // prefix survives.
        // ADR 0026 D7: a `/model` switch moves the budget with the model. `None` keeps the
        // configured budget, so a session that never switches behaves exactly as before.
        options.context_budget = self.turn_budget;
        // ADR 0018: the same switch moves the compaction call's wire settings.
        options.compaction_settings = self.turn_compaction_settings.clone();
        if let Some(timeout) = self.turn_timeout {
            options.deadline = Some(tokio::time::Instant::now() + timeout);
        }
        journal
            .append_runtime_event(RuntimeEvent::InputClaimed {
                message_ids: vec![entry.id.clone()],
                turn_id: turn_id.clone(),
            })
            .await?;
        let started = RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
        };
        journal.append_runtime_event(started.clone()).await?;
        // The host hears the start once it is durable, like the end (a task's `running` flag
        // and the control port's event stream key on it).
        if let Some(sink) = &runtime_events {
            sink.on_runtime_event(&started);
        }

        let mut claimed: Vec<MessageId> = vec![entry.id.clone()];
        // Every claimed input, so an accepted-but-unapplied steer can go back to the inbox
        // (ADR 0011: no input is dropped). `entry` is cloned before its message moves into
        // the run.
        let mut claimed_entries: Vec<InboxEntry> = vec![entry.clone()];
        let mut admitted: HashSet<MessageId> = HashSet::new();
        let tap = Tap::new(&*live);

        // The addition seam needs the turn's own text and source after the message has moved
        // into the run (ADR 0021).
        let input_text = entry.message.text();
        let input_source = entry.source;

        let mut run = self.session.start_turn_with(entry.message, options)?;
        let steering = run.steering_handle();
        let cancellation = run.cancellation_handle();
        // ADR 0021: a host may add one message to a **user** turn. It
        // goes into the `inject` lane, so it is a durable input with an id, and the
        // `drain_steering` below hands it to the core before the turn's first model call —
        // right after the user message in the transcript. The host's callback sees the
        // transcript as it is now, which is the dedup ledger. A refused inject is dropped: the
        // addition is context, never a session fact (ADR 0021 invariant 2).
        if let (Some(additions), InputSource::User) = (turn_additions.as_ref(), input_source)
            && let Some(message) = additions(&input_text, run.transcript())
        {
            let _ = inbox.inject(message, InputSource::System).await;
        }
        // Anything already waiting in the next_step lane joins before the first request.
        drain_steering(
            &inbox,
            &journal,
            &steering,
            &turn_id,
            &mut claimed,
            &mut claimed_entries,
        )
        .await?;

        let cancel_generation = inbox.cancel_generation();
        #[cfg(feature = "testing")]
        let wake_branch = Arc::clone(&self.testing_wake_branch);
        // ADR 0017: a model or hook error ends **this turn**, not the session. Every other
        // `RunError` (journal, poisoned, session) is still fatal and propagates here.
        let outcome = loop {
            let mut step = Box::pin(run.step(&tap));
            let result = loop {
                tokio::select! {
                    result = &mut step => break result,
                    _ = inbox.wake_signal() => {
                        #[cfg(feature = "testing")]
                        wake_branch.notify_waiters();
                        // Only cancellation is handled here. `step` is not polled while this
                        // branch body runs, so it can be parked in the middle of a core
                        // replacement segment (`TranscriptReplaced` written, commit not yet).
                        // Any journal write here (`drain_steering` -> `InputClaimed`) would
                        // wait on the writer's segment gate, which only closes when `step` is
                        // polled again: a deadlock. Steering is instead drained after
                        // `Ok(Step::Continue)`, which is where the core reads it anyway
                        // (`inject_steering` runs at the start of a step); a steer left in the
                        // inbox at the end of a turn opens the next one itself (ADR 0011
                        // revision 1 decision 2), and every older `inject` in the lane is
                        // absorbed by that turn's first step.
                        if inbox.cancel_generation() != cancel_generation {
                            cancellation.cancel();
                        }
                    }
                }
            };
            admit_seen(&journal, &claimed, &mut admitted, &tap, &runtime_events).await?;
            match result {
                Ok(Step::Continue) => {
                    drain_steering(
                        &inbox,
                        &journal,
                        &steering,
                        &turn_id,
                        &mut claimed,
                        &mut claimed_entries,
                    )
                    .await?;
                }
                Ok(Step::Finished(outcome)) => break Ok(outcome),
                // ADR 0017: a model error ends the turn; every other error ends the session.
                Err(error) if is_fatal(&error) => return Err(error.into()),
                Err(error) => break Err(error),
            }
        };
        drop(run);
        admit_seen(&journal, &claimed, &mut admitted, &tap, &runtime_events).await?;
        // A steer Core accepted but never applied (the turn ended first) goes back to the
        // inbox front, in order, so the next turn uses it (ADR 0011). This covers both the
        // normal finish and the model-error finish (ADR 0017).
        requeue_unadmitted(&inbox, &claimed_entries, &admitted).await;

        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                // A model or hook error. The adapter already masked its own key and header
                // values; the host redacts the text against its own secret list before
                // showing it.
                let message = error.to_string();
                let failed = RuntimeEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    message,
                };
                journal.append_runtime_event(failed.clone()).await?;
                if let Some(sink) = &runtime_events {
                    sink.on_runtime_event(&failed);
                }
                if let Some(delay) = self.commit_delay {
                    tokio::time::sleep(delay).await;
                }
                let finished = RuntimeEvent::TurnFinished {
                    turn_id,
                    outcome: TurnOutcome::Failed,
                    reason: None,
                    stop_reason: None,
                };
                journal.append_runtime_event(finished.clone()).await?;
                // The host hears the journaled turn boundary (ADR 0023 U5): it flips its
                // "the turn is on disk" flag, so a `/fork` cannot race the append.
                if let Some(sink) = &runtime_events {
                    sink.on_runtime_event(&finished);
                }
                // The driver keeps running: the next input opens a fresh turn.
                return Ok(());
            }
        };

        if let Some(delay) = self.commit_delay {
            tokio::time::sleep(delay).await;
        }
        let (outcome, reason, stop_reason) = wire_outcome(&outcome);
        let finished = RuntimeEvent::TurnFinished {
            turn_id,
            outcome,
            reason,
            stop_reason,
        };
        journal.append_runtime_event(finished.clone()).await?;
        // Journaled before the host hears it: the host flips its "the turn is on disk" flag
        // here, so a `/fork` that arrives first waits for this event.
        if let Some(sink) = &runtime_events {
            sink.on_runtime_event(&finished);
        }
        Ok(())
    }

    /// Runs one queued control request (ADR 0018: `/compact`). `step` is not running, so no
    /// replacement segment is open and the writer's gate is free; the only journal writes are
    /// the compaction's own. `cancel` (Ctrl-C/Esc) cancels the in-flight summarizer and
    /// commits nothing.
    async fn run_control(&mut self, request: ControlRequest) -> Result<(), DriverError> {
        let inbox = Arc::clone(&self.inbox);
        // A new cancellation episode: the next `cancel` counts as a first cancel again.
        inbox.begin_turn();
        let generation = inbox.cancel_generation();
        let token = CancellationToken::new();
        let live = Arc::clone(&self.live);
        // The compaction settings in force (ADR 0018): cloned before the mutable
        // borrow of the session below.
        let settings = self.turn_compaction_settings.clone();
        let session = &mut self.session;
        let compact = session.compact_now(
            request.instructions,
            &token,
            &*live,
            request.budget,
            settings,
        );
        tokio::pin!(compact);
        let result = loop {
            tokio::select! {
                biased;
                _ = inbox.wake_signal() => {
                    if inbox.cancel_generation() != generation {
                        token.cancel();
                    }
                }
                result = &mut compact => break result,
            }
        };
        match result {
            Ok(outcome) => {
                let _ = request.reply.send(Ok(outcome));
            }
            Err(failure) => {
                // The caller gets a readable reason; a journal failure is fatal here as it is
                // for a turn (the file and memory may differ).
                let message = failure.to_string();
                let _ = request.reply.send(Err(message));
                if let CompactionFailure::Journal(error) = failure {
                    return Err(DriverError::CoreJournal(error));
                }
            }
        }
        Ok(())
    }

    async fn apply_pending_config(&mut self) -> Result<(), DriverError> {
        for change in self.queue.drain() {
            let revision = self.next_config_revision;
            self.next_config_revision += 1;
            let event = RuntimeEvent::ConfigChanged {
                revision,
                change: change.clone(),
            };
            self.journal.append_runtime_event(event.clone()).await?;
            // The host hears the applied change like any other runtime event: the UI's
            // `next:` marker (D5) is spent exactly when the change is durable.
            if let Some(sink) = &self.runtime_events {
                sink.on_runtime_event(&event);
            }
            self.apply_change(change, revision);
        }
        Ok(())
    }

    /// Applies one change to the slot, and for a model change recomputes the budget the next
    /// turn runs under (ADR 0026 D7). Also used by resume replay (D8).
    fn apply_change(&mut self, change: serde_json::Value, revision: u64) {
        // A change **merges** into the slot's object (D11): a `{"thinking": …}` change must
        // survive the next model change and the reverse — the switchable model reads the
        // whole current state. A non-object change replaces.
        let mut merged = self.slot.value();
        merge_change(&mut merged, &change);
        self.slot.apply(merged, revision);
        if let Some(route) = ModelRoute::from_value(&change) {
            if let Some(resolver) = &self.budget_for {
                self.turn_budget = resolver(&route.provider, &route.model);
            }
            if let Some(resolver) = &self.compaction_settings_for {
                self.turn_compaction_settings = Some(resolver(&route.provider, &route.model));
            }
        }
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
    claimed_entries: &mut Vec<InboxEntry>,
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
            wake,
            source,
            message,
            seq,
        } = entry;
        match steering.steer(message.clone()) {
            Ok(()) => {
                claimed.push(id.clone());
                // Core accepted the steer, but it is only applied at the next safe point. If
                // the turn ends first, the entry goes back to the inbox (see
                // `requeue_unadmitted`), so an accepted steer is never dropped in-process.
                claimed_entries.push(InboxEntry {
                    id,
                    lane,
                    wake,
                    source,
                    message,
                    seq,
                });
            }
            Err(agent_core::SteerError::RunFinished(message)) => {
                // The turn is winding down: put it back so it is not lost. The claim above
                // stays truthful until recovery requeues it (ADR 0011 L3-2).
                inbox
                    .requeue_front(InboxEntry {
                        id,
                        lane,
                        wake,
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

/// Puts every claimed input that never reached the transcript back at the front of its lane,
/// in claim order, so the next turn uses it. A steer Core accepted but never applied (the
/// turn ended first) lands here; without it the input would be lost until a restart. The
/// journal already has `InputClaimed` for it and no `InputAdmitted`, which is exactly what
/// `recover` requeues, so the record stays truthful.
///
/// A requeued **steer** keeps `wake = true`, so the loop iteration right after this — before
/// the driver waits — claims it and opens the next turn with it as its first input (ADR 0011
/// revision 1 decisions 2 and 3). A requeued `inject` keeps `wake = false` and waits.
async fn requeue_unadmitted(inbox: &Inbox, claimed: &[InboxEntry], admitted: &HashSet<MessageId>) {
    // Front-most last: a single front-push per entry restores the original order.
    for entry in claimed.iter().rev() {
        if !admitted.contains(&entry.id) {
            inbox.requeue_front(entry.clone()).await;
        }
    }
}

/// Writes `InputAdmitted` for every claimed id the transcript has actually taken, and
/// forwards each one to the host sink (a host that shows the queue hears the admission live).
async fn admit_seen(
    journal: &SessionJournal,
    claimed: &[MessageId],
    admitted: &mut HashSet<MessageId>,
    tap: &Tap<'_>,
    sink: &Option<Arc<dyn RuntimeEvents>>,
) -> Result<(), DriverError> {
    for id in claimed {
        if admitted.contains(id) {
            continue;
        }
        if let Some(index) = tap.index_of(id) {
            let event = RuntimeEvent::InputAdmitted {
                message_id: id.clone(),
                transcript_index: index,
            };
            journal.append_runtime_event(event.clone()).await?;
            if let Some(sink) = sink {
                sink.on_runtime_event(&event);
            }
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

/// ADR 0017: a model error **or a hook error** ends the turn instead of the session.
/// Journal, poisoned and session errors stay fatal — the session state may be damaged or
/// unusable, so the driver must stop rather than keep writing to it. This is the single place
/// the policy lives; `run_turn` calls it.
///
/// A hook error leaves a valid transcript: `before_model_call` and `after_model_response` are
/// the only hook points that fail a turn, and both run before the assistant message of the
/// step is committed. The tool-call hooks fail closed instead (they resolve the call with a
/// `hook_error` result and continue), so they never reach here.
fn is_fatal(error: &RunError) -> bool {
    !matches!(
        error,
        RunError::Model(_) | RunError::Hook { .. } | RunError::Compaction(_)
    )
}

/// Maps the core's `RunStatus` to the ADR 0006 §5.6 wire shape.
fn wire_outcome(outcome: &RunOutcome) -> (TurnOutcome, Option<TurnReason>, Option<String>) {
    match &outcome.status {
        RunStatus::Completed => (TurnOutcome::Completed, None, None),
        RunStatus::Refused => (TurnOutcome::Refused, None, None),
        RunStatus::BudgetExceeded => (TurnOutcome::BudgetExceeded, None, None),
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
///    durable, inputs that were enqueued but never claimed before the restart; each keeps its
///    durable `wake`, so a pending steering entry opens a turn as soon as the driver runs
///    (ADR 0011). An old journal has no `wake`: the reader derives it from the lane
///    (ADR 0006 §5.5);
/// 3. unpaired tool calls go through the core's `repair` (ADR 0002), and the repaired
///    messages are written back before the session is reopened (ADR 0002 E9).
///
/// Merges one `ConfigChanged` change into a config state (ADR 0026 D8/D11): object keys are
/// inserted, anything else replaces. One function for the live path and the resume replay, so
/// the two cannot drift (review M3).
fn merge_change(state: &mut serde_json::Value, change: &serde_json::Value) {
    match (state.as_object_mut(), change.as_object()) {
        (Some(state), Some(partial)) => {
            for (key, value) in partial {
                state.insert(key.clone(), value.clone());
            }
        }
        _ => *state = change.clone(),
    }
}

async fn recover(
    journal: &SessionJournal,
    parsed: Option<&radian_session::Session>,
) -> Result<Recovery, DriverError> {
    let t0 = std::time::Instant::now();
    let owned;
    let parsed = match parsed {
        Some(parsed) => parsed,
        None => {
            owned = Session::open(journal.path())?;
            &owned
        }
    };
    let records = parsed.journal_records();
    radian_session::trace::took("recover/open", t0);
    let t1 = std::time::Instant::now();
    let replayed = agent_core::replay(&records)?;
    radian_session::trace::took("recover/replay", t1);
    let t2 = std::time::Instant::now();
    let history = agent_core::recover(&records)?;
    radian_session::trace::took("recover/recover", t2);
    let t3 = std::time::Instant::now();

    // E9: the repaired tail must be durable before the session is opened with it.
    for (offset, message) in history[replayed.len()..].iter().enumerate() {
        journal
            .append(JournalRecord::Message {
                index: replayed.len() + offset,
                message: message.clone(),
            })
            .await?;
    }

    radian_session::trace::took("recover/tail", t3);
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

    let mut enqueued: Vec<(MessageId, InboxLane, Message, u64, bool)> = Vec::new();
    let mut admitted: HashSet<MessageId> = HashSet::new();
    let mut recorded_admitted: HashSet<MessageId> = HashSet::new();
    let mut next_turn = 1u64;
    // ADR 0006 §5.7: the session-local config revision starts at 1.
    let mut next_config_revision = 1u64;
    // D8 + D11 (review M3): merge every change in revision order, exactly like `apply_change`,
    // so no change of any kind can erase another.
    let mut config_state: Option<(serde_json::Value, u64)> = None;
    for event in parsed.events() {
        match &event.data {
            EventData::InboxEnqueued {
                message,
                message_id,
                lane,
                wake,
                ..
            } => enqueued.push((message_id.clone(), *lane, message.clone(), event.seq, *wake)),
            EventData::InputAdmitted { message_id, .. } => {
                recorded_admitted.insert(message_id.clone());
                admitted.insert(message_id.clone());
            }
            EventData::TurnStarted { turn_id } | EventData::TurnFinished { turn_id, .. } => {
                next_turn = next_turn.max(turn_number(turn_id) + 1);
            }
            EventData::ConfigChanged { revision, change } => {
                next_config_revision = next_config_revision.max(revision + 1);
                let merged = config_state.get_or_insert_with(|| (serde_json::json!({}), 0));
                merge_change(&mut merged.0, change);
                merged.1 = *revision;
            }
            _ => {}
        }
    }

    // Catch the admission record up for anything the transcript already holds, so the
    // journal and the transcript agree before the session is reopened.
    for (id, _, _, _, _) in &enqueued {
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
    for (id, lane, message, seq, wake) in enqueued.into_iter().rev() {
        if !admitted.contains(&id) {
            requeue.push(InboxEntry {
                id,
                lane: lane_of(lane),
                wake,
                source: source_of(&message),
                message,
                seq,
            });
        }
    }
    Ok(Recovery {
        config_state,
        history,
        requeue,
        next_turn,
        next_config_revision,
        budget_baseline: reported_baseline(parsed),
        transcript_revision: parsed.last_committed_revision(),
    })
}

/// The last provider-reported **prompt size** in the **current** transcript revision, with the
/// length of the transcript that produced it (ADR 0015 C1).
///
/// The number is `ModelUsage::prompt_total()`, not `input`: the whole prompt the provider saw,
/// cache reads and writes included (ADR 0007). For an Anthropic record `input` is
/// only the *uncached* part, so a baseline taken from it would understate the request and make
/// the budget act too late; for an OpenAI-compatible record `input` already contains the cache
/// hit, and `prompt_total()` is that same number.
///
/// `ModelUsageRecorded` follows the assistant `MessageAppended` of the response it priced
/// (ADR 0007 J5), and a request's `messages` is exactly the transcript before that assistant
/// message. So the request prefix length is the preceding assistant message's `index`. A
/// `TranscriptReplaced` starts a new revision whose indexes restart at zero; an earlier
/// revision's usage must not be paired with the new transcript, so both trackers reset there.
/// A usage event without a preceding assistant message, or with no prompt size (ADR 0007: not
/// reported, not zero), is skipped; an earlier report that does have one stays usable.
fn reported_baseline(session: &Session) -> Option<(usize, u64)> {
    let mut last_assistant: Option<usize> = None;
    let mut baseline = None;
    for event in session.events() {
        match &event.data {
            EventData::TranscriptReplaced { .. } => {
                last_assistant = None;
                baseline = None;
            }
            EventData::MessageAppended { index, message } => {
                last_assistant = match message {
                    Message::Assistant { .. } => Some(*index as usize),
                    _ => None,
                };
            }
            EventData::ModelUsageRecorded { usage, .. } => {
                if let (Some(input), Some(prefix_len)) = (usage.prompt_total(), last_assistant) {
                    baseline = Some((prefix_len, input));
                }
            }
            _ => {}
        }
    }
    baseline
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{
        CompactionError, HookError, HookPoint, Message, ModelError, ModelUsage, SessionError,
    };

    /// ADR 0017: the classification the driver uses. A model or hook error is non-fatal;
    /// journal, poisoned and session errors must keep ending the session.
    #[test]
    fn only_model_and_hook_errors_are_non_fatal() {
        assert!(!is_fatal(&RunError::Model(ModelError::Transport(
            "http 500".into()
        ))));
        assert!(!is_fatal(&RunError::Hook {
            point: HookPoint::BeforeModelCall,
            error: HookError("hook refused".into()),
        }));
        assert!(is_fatal(&RunError::Journal(JournalError(
            "write failed".into()
        ))));
        // ADR 0018: a compaction failure fails this turn like a model error, not the session.
        assert!(!is_fatal(&RunError::Compaction(
            CompactionError::Insufficient {
                before: 10,
                after: 10,
                target: 5,
            }
        )));
        assert!(is_fatal(&RunError::Poisoned));
        assert!(is_fatal(&RunError::Session(SessionError::Poisoned)));
    }

    /// ADR 0018: the budget baseline comes only from `ModelUsageRecorded` after the last
    /// committed replacement; a pre-replacement report and a `CompactionUsageRecorded` are
    /// both ignored.
    #[tokio::test]
    async fn the_baseline_ignores_usage_before_a_replacement() {
        use agent_core::{JournalRecord, TranscriptJournal};
        use radian_session::{SessionCreatedData, SessionJournal};

        let dir = std::env::temp_dir().join(format!("radian-baseline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let journal = SessionJournal::create(
            &path,
            "s",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        let usage = |input: u64| ModelUsage {
            input: Some(input),
            ..Default::default()
        };
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("old"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::assistant("a"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::ModelUsage { usage: usage(500) })
            .await
            .unwrap();
        journal
            .append(JournalRecord::TranscriptReplaced {
                revision: 1,
                len: 1,
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("summary"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::TranscriptReplacementCommitted {
                revision: 1,
                len: 1,
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::CompactionUsage {
                revision: 2,
                model: "current".into(),
                usage: usage(999),
            })
            .await
            .unwrap();
        drop(journal);

        let session = Session::open(&path).unwrap();
        assert_eq!(
            reported_baseline(&session),
            None,
            "the pre-replacement usage and the compaction usage are not baselines"
        );
        assert_eq!(session.last_committed_revision(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// ADR 0007 + ADR 0015 C1: the restored baseline is the **prompt total**, so an
    /// Anthropic-shaped record (where `input` is only the uncached part) yields
    /// `input + cache_read + cache_write`; an OpenAI-shaped one yields `input` itself, because
    /// there the cache hit is already inside it. An unmarked old record is taken at face value.
    #[tokio::test]
    async fn the_restored_baseline_is_the_prompt_total_not_the_uncached_input() {
        use agent_core::{JournalRecord, TranscriptJournal};
        use radian_session::{SessionCreatedData, SessionJournal};

        let dir = std::env::temp_dir().join(format!("radian-prompt-total-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let journal = SessionJournal::create(
            &path,
            "s",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        let record = |index: usize, usage: ModelUsage| {
            let journal = &journal;
            async move {
                journal
                    .append(JournalRecord::Message {
                        index: index * 2,
                        message: Message::user("u"),
                    })
                    .await
                    .unwrap();
                journal
                    .append(JournalRecord::Message {
                        index: index * 2 + 1,
                        message: Message::assistant("a"),
                    })
                    .await
                    .unwrap();
                journal
                    .append(JournalRecord::ModelUsage { usage })
                    .await
                    .unwrap();
            }
        };
        // The Anthropic shape: 10 000 uncached + 190 000 read + 1 000 written.
        record(
            0,
            ModelUsage {
                input: Some(10_000),
                cache_read: Some(190_000),
                cache_write: Some(1_000),
                input_includes_cache: Some(false),
                ..Default::default()
            },
        )
        .await;
        // The OpenAI shape: `input` already contains the 190 000 cached tokens.
        record(
            1,
            ModelUsage {
                input: Some(200_000),
                cache_read: Some(190_000),
                input_includes_cache: Some(true),
                ..Default::default()
            },
        )
        .await;
        drop(journal);

        // The **last** report in the revision wins, and it is the OpenAI one: 200 000, not
        // 390 000 (the cache hit is inside `input`).
        let session = Session::open(&path).unwrap();
        assert_eq!(reported_baseline(&session), Some((3, 200_000)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same, with only the Anthropic-shaped record: the baseline must be 201 000, not the
    /// 10 000 that `input` alone would give (the deviation ADR 0007 asks to fix).
    #[tokio::test]
    async fn an_anthropic_shaped_report_restores_the_whole_prompt_as_its_baseline() {
        use agent_core::{JournalRecord, TranscriptJournal};
        use radian_session::{SessionCreatedData, SessionJournal};

        let dir = std::env::temp_dir().join(format!("radian-prompt-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let journal = SessionJournal::create(
            &path,
            "s",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("u"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::assistant("a"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::ModelUsage {
                usage: ModelUsage {
                    input: Some(10_000),
                    cache_read: Some(190_000),
                    cache_write: Some(1_000),
                    input_includes_cache: Some(false),
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        drop(journal);

        let session = Session::open(&path).unwrap();
        assert_eq!(reported_baseline(&session), Some((1, 201_000)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
