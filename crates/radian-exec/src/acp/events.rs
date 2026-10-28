//! Live events to `session/update` (ADR 0049), and the runtime events the prompt response
//! needs.
//!
//! The sink runs synchronously inside the turn loop: it only appends to the writer's bounded
//! channel and takes one short lock. When the queue is full it waits (the turn stalls) instead
//! of dropping an update, while durable journal writes are untouched. A shutdown releases the
//! wait, so the driver can still join when the client stopped reading.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use radian_core::{AgentEvent, EventSink, Message, MessageId, ModelUsage, RunStatus};
use tokio::sync::Notify;

use super::protocol;
use super::wire::Outbox;

/// How many finished turns the ledger remembers. A prompt reads its turn's end right after the
/// notification, so one turn of slack is enough; the margin only has to cover a prompt task
/// that the scheduler delays, and each turn it could be delayed through is at least one
/// provider round trip.
const FINISHED_TURNS_KEPT: usize = 64;

/// What the two sinks know together about turns (ADR 0049). The live sink sees `RunFinished`
/// (a status, no turn id); the runtime sink sees `InputClaimed`, `TurnStarted` and
/// `TurnFinished` (ids, no status). One ledger joins them, so a prompt is answered by the turn
/// that claimed **its** message and never by a turn that ran before it.
///
/// Turns are serial in one session, so the live end always belongs to the driver's current
/// turn (`TurnStarted` until its `TurnFinished`), which is how a status is attributed.
pub(crate) struct TurnLedger {
    inner: Mutex<LedgerInner>,
    /// Wakes a prompt that waits for its turn's journaled end. `notify_one` keeps a permit, so
    /// a notification that lands between a check and the wait is not lost.
    changed: Notify,
}

#[derive(Default)]
struct LedgerInner {
    /// The driver's turn in flight (`TurnStarted`), cleared by its `TurnFinished`.
    current: Option<String>,
    /// Message id -> the turn that claimed it (`InputClaimed`), merged inputs included.
    claimed: HashMap<MessageId, String>,
    /// Turn id -> the status of its live `RunFinished`.
    live: HashMap<String, RunStatus>,
    /// Turn id -> the masked `TurnFailed` message that failed it, so a prompt reads its own
    /// turn's error and not a later turn's.
    errors: HashMap<String, String>,
    /// The turns whose end the driver has journaled (`TurnFinished`), newest last.
    journaled: HashSet<String>,
    /// The same turn ids in the order they finished, so the oldest can be forgotten.
    order: VecDeque<String>,
}

impl TurnLedger {
    fn new() -> Self {
        Self {
            inner: Mutex::new(LedgerInner::default()),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, LedgerInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The core's live turn end. The status belongs to the driver's turn in flight: turns are
    /// serial, so it cannot belong to an older one.
    fn live_finished(&self, status: RunStatus) {
        let mut inner = self.lock();
        if let Some(turn) = inner.current.clone() {
            inner.live.insert(turn, status);
        }
    }

    /// A turn failed on a model or hook error (ADR 0017): keep the text against **that** turn,
    /// so a prompt reports its own turn's error even when a later turn fails before the answer
    /// task runs.
    fn turn_failed(&self, turn: &str, message: String) {
        self.lock().errors.insert(turn.to_owned(), message);
    }

    /// The runtime events a host receives, in the order the driver writes them (ADR 0049).
    fn on_runtime(&self, event: &radian_session::RuntimeEvent) {
        let journaled = {
            let mut inner = self.lock();
            match event {
                radian_session::RuntimeEvent::InputClaimed {
                    message_ids,
                    turn_id,
                } => {
                    for id in message_ids {
                        inner.claimed.insert(id.clone(), turn_id.clone());
                    }
                    false
                }
                radian_session::RuntimeEvent::TurnStarted { turn_id } => {
                    inner.current = Some(turn_id.clone());
                    false
                }
                radian_session::RuntimeEvent::TurnFinished { turn_id, .. } => {
                    if inner.current.as_deref() == Some(turn_id.as_str()) {
                        inner.current = None;
                    }
                    if inner.journaled.insert(turn_id.clone()) {
                        inner.order.push_back(turn_id.clone());
                    }
                    inner.forget_oldest();
                    true
                }
                _ => false,
            }
        };
        if journaled {
            self.changed.notify_one();
        }
    }

    /// The turn that claimed `message`, once the driver has journaled the claim.
    pub(crate) fn turn_for(&self, message: &MessageId) -> Option<String> {
        self.lock().claimed.get(message).cloned()
    }

    /// Whether `turn`'s end is journaled.
    pub(crate) fn is_journaled(&self, turn: &str) -> bool {
        self.lock().journaled.contains(turn)
    }

    /// The status of `turn`'s live end, if the core reported one.
    pub(crate) fn status(&self, turn: &str) -> Option<RunStatus> {
        self.lock().live.get(turn).cloned()
    }

    /// The masked error that failed `turn`, if it ended on a model or hook error.
    pub(crate) fn error(&self, turn: &str) -> Option<String> {
        self.lock().errors.get(turn).cloned()
    }

    /// Resolves when a turn end is journaled.
    pub(crate) fn changed(&self) -> &Notify {
        &self.changed
    }
}

impl LedgerInner {
    /// Forgets the oldest finished turn once the history is longer than the margin. A prompt's
    /// turn is read within one turn of its end, so this only drops what nothing waits for.
    fn forget_oldest(&mut self) {
        while self.order.len() > FINISHED_TURNS_KEPT {
            let Some(turn) = self.order.pop_front() else {
                return;
            };
            self.journaled.remove(&turn);
            self.live.remove(&turn);
            self.errors.remove(&turn);
            self.claimed.retain(|_, claimed| claimed != &turn);
        }
    }
}

/// The host's live-event sink for one ACP session.
pub(crate) struct AcpEvents {
    out: Outbox,
    session_id: String,
    workspace: PathBuf,
    /// `C`, the context window, when known (ADR 0018). Without it `usage_update` is not sent:
    /// inventing a size would be worse than staying silent.
    window: Option<u64>,
    busy: AtomicBool,
    ledger: Arc<TurnLedger>,
    /// `call_id -> (tool name, arguments)` from the committed assistant message, so
    /// `ToolStarted` can report the title, kind, locations and raw input.
    tool_calls: Mutex<HashMap<String, (String, serde_json::Value)>>,
}

impl AcpEvents {
    pub(crate) fn new(
        out: Outbox,
        session_id: String,
        workspace: PathBuf,
        window: Option<u64>,
        ledger: Arc<TurnLedger>,
    ) -> Self {
        Self {
            out,
            session_id,
            workspace,
            window,
            busy: AtomicBool::new(false),
            ledger,
            tool_calls: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    fn notify(&self, update: serde_json::Value) {
        // The sink is called synchronously from the turn loop, so it uses the outbox's blocking
        // path: a full queue stalls the turn until the client reads, and drops nothing.
        self.out.notify_blocking("session/update", {
            serde_json::json!({
                "sessionId": self.session_id,
                "update": update,
            })
        });
    }

    fn usage(&self, usage: &ModelUsage) {
        let Some(input) = usage.prompt_total() else {
            return;
        };
        let Some(size) = self.window else {
            return;
        };
        let used = input.saturating_add(usage.output.unwrap_or(0));
        self.notify(serde_json::json!({
            "sessionUpdate": "usage_update",
            "used": used,
            "size": size,
        }));
    }

    fn remember_tool_calls(&self, calls: &[radian_core::ToolCall]) {
        let mut known = self.tool_calls.lock().unwrap();
        for call in calls {
            known.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
        }
    }
}

impl EventSink for AcpEvents {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted => {
                self.busy.store(true, Ordering::Release);
                self.tool_calls.lock().unwrap().clear();
            }
            AgentEvent::ModelTextDelta { text } => {
                self.notify(protocol::text_chunk("agent_message_chunk", &text));
            }
            AgentEvent::ModelReasoningDelta { text } => {
                self.notify(protocol::text_chunk("agent_thought_chunk", &text));
            }
            AgentEvent::TranscriptAppended { message, .. } => match message {
                Message::Assistant { tool_calls, .. } => {
                    self.remember_tool_calls(&tool_calls);
                }
                Message::Tool { result, .. } => {
                    let name = self
                        .tool_calls
                        .lock()
                        .unwrap()
                        .get(&result.call_id)
                        .map(|(name, _)| name.clone())
                        .unwrap_or_default();
                    self.notify(protocol::tool_call_update(
                        &result.call_id,
                        &name,
                        result.is_error,
                        Some(protocol::tool_output(&result)),
                    ));
                }
                Message::User { .. } => {}
            },
            AgentEvent::ToolStarted { call_id, name, .. } => {
                let known = self.tool_calls.lock().unwrap().get(&call_id).cloned();
                let (name, arguments) = known.unwrap_or((name, serde_json::Value::Null));
                self.notify(protocol::tool_call(
                    &call_id,
                    &name,
                    &arguments,
                    &self.workspace,
                ));
            }
            AgentEvent::ToolFinished {
                call_id, is_error, ..
            } => {
                let name = self
                    .tool_calls
                    .lock()
                    .unwrap()
                    .get(&call_id)
                    .map(|(name, _)| name.clone())
                    .unwrap_or_default();
                self.notify(protocol::tool_call_update(&call_id, &name, is_error, None));
            }
            AgentEvent::ModelResponseReceived {
                usage: Some(usage), ..
            } => self.usage(&usage),
            AgentEvent::CompactionUsage { usage, .. } => self.usage(&usage),
            AgentEvent::RunFinished { status } => {
                self.busy.store(false, Ordering::Release);
                self.ledger.live_finished(status);
            }
            _ => {}
        }
    }
}

/// The runtime-event sink: keeps each failed turn's error text for `_meta.radian.error`,
/// masked with the same secrets the session redacts everywhere else, and the [`TurnLedger`].
///
/// The prompt answer waits for the journaled end of the turn that claimed **its** message,
/// because the core's live `RunFinished` fires before the driver commits the turn (ADR 0010,
/// ADR 0049), and because a turn that ran before the prompt's own (a job-completion wake turn,
/// ADR 0037) must not answer it. The error and the turn's fee and text are read by turn id
/// (ADR 0049): a later turn can start and finish before the answer task runs, and none of its
/// facts may stand in for the prompt's turn.
pub(crate) struct AcpRuntimeEvents {
    secrets: Vec<String>,
    ledger: Arc<TurnLedger>,
}

impl AcpRuntimeEvents {
    pub(crate) fn new(secrets: Vec<String>, ledger: Arc<TurnLedger>) -> Self {
        Self { secrets, ledger }
    }

    /// The turn ledger the prompt driver waits on.
    pub(crate) fn ledger(&self) -> &Arc<TurnLedger> {
        &self.ledger
    }
}

impl radian_runtime::RuntimeEvents for AcpRuntimeEvents {
    fn on_runtime_event(&self, event: &radian_session::RuntimeEvent) {
        if let radian_session::RuntimeEvent::TurnFailed { turn_id, message } = event {
            // `TurnFailed` is not the turn end: the driver journals it, then `TurnFinished`
            // with the failed outcome, and the answer must wait for that record. Keeping it
            // per turn keeps it out of a later turn's answer.
            self.ledger
                .turn_failed(turn_id, crate::redact::redact(message, &self.secrets));
        }
        self.ledger.on_runtime(event);
    }
}

/// Builds the live sink and the runtime sink over one ledger, so the live status and the
/// journaled turn ids meet.
pub(crate) fn events(
    out: Outbox,
    session_id: String,
    workspace: PathBuf,
    window: Option<u64>,
    secrets: Vec<String>,
) -> (Arc<AcpEvents>, Arc<AcpRuntimeEvents>) {
    let ledger = Arc::new(TurnLedger::new());
    (
        Arc::new(AcpEvents::new(
            out,
            session_id,
            workspace,
            window,
            Arc::clone(&ledger),
        )),
        Arc::new(AcpRuntimeEvents::new(secrets, ledger)),
    )
}

#[cfg(test)]
mod tests {
    use super::TurnLedger;

    /// ADR 0049: `_meta.radian.error` belongs to the turn that failed. Once the prompt's
    /// `TurnFinished` has woken the answer task, the driver can run and fail a later wake turn
    /// before that task reads the ledger, so the error must be kept per turn.
    #[test]
    fn a_later_turn_failure_does_not_replace_an_earlier_turns_error() {
        let ledger = TurnLedger::new();
        ledger.turn_failed("t-1", "the prompt's turn failed".to_owned());
        ledger.turn_failed("t-2", "the wake turn failed".to_owned());
        assert_eq!(
            ledger.error("t-1").as_deref(),
            Some("the prompt's turn failed")
        );
        assert_eq!(ledger.error("t-2").as_deref(), Some("the wake turn failed"));
        assert_eq!(ledger.error("t-3"), None);
    }
}
