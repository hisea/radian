//! Live events to `session/update` (ADR 0049), and the runtime events the prompt response
//! needs.
//!
//! The sink runs synchronously inside the turn loop: it only appends to the writer's channel
//! and takes one short lock, so a slow client never stalls a turn and durable journal writes
//! are untouched.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use radian_core::{AgentEvent, EventSink, Message, ModelUsage, RunStatus};
use tokio::sync::Notify;

use super::protocol;
use super::wire::Outbox;

/// The host's live-event sink for one ACP session.
pub(crate) struct AcpEvents {
    out: Outbox,
    session_id: String,
    workspace: PathBuf,
    /// `C`, the context window, when known (ADR 0018). Without it `usage_update` is not sent:
    /// inventing a size would be worse than staying silent.
    window: Option<u64>,
    busy: AtomicBool,
    /// Wakes when a turn starts.
    pub(crate) started: Notify,
    /// Wakes when a turn ends.
    pub(crate) finished: Notify,
    /// How many turns have ended. The prompt driver waits for it to move past the value it
    /// read before enqueueing, so a stale `finished` permit can never be mistaken for this
    /// turn's end.
    finished_generation: AtomicU64,
    last_status: Mutex<Option<RunStatus>>,
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
    ) -> Self {
        Self {
            out,
            session_id,
            workspace,
            window,
            busy: AtomicBool::new(false),
            started: Notify::new(),
            finished: Notify::new(),
            finished_generation: AtomicU64::new(0),
            last_status: Mutex::new(None),
            tool_calls: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub(crate) fn finished_generation(&self) -> u64 {
        self.finished_generation.load(Ordering::Acquire)
    }

    pub(crate) fn last_status(&self) -> Option<RunStatus> {
        self.last_status.lock().unwrap().clone()
    }

    fn notify(&self, update: serde_json::Value) {
        self.out.notify("session/update", {
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
                self.started.notify_waiters();
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
                *self.last_status.lock().unwrap() = Some(status);
                self.finished_generation.fetch_add(1, Ordering::AcqRel);
                self.finished.notify_waiters();
            }
            _ => {}
        }
    }
}

/// The runtime-event sink: keeps the last turn's failure text for `_meta.radian.error`,
/// masked with the same secrets the session redacts everywhere else. `TurnStarted` clears it,
/// so only the turn that just ended can contribute.
pub(crate) struct AcpRuntimeEvents {
    error: Mutex<Option<String>>,
    secrets: Vec<String>,
}

impl AcpRuntimeEvents {
    pub(crate) fn new(secrets: Vec<String>) -> Self {
        Self {
            error: Mutex::new(None),
            secrets,
        }
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }
}

impl radian_runtime::RuntimeEvents for AcpRuntimeEvents {
    fn on_runtime_event(&self, event: &radian_session::RuntimeEvent) {
        match event {
            radian_session::RuntimeEvent::TurnFailed { message, .. } => {
                *self.error.lock().unwrap() = Some(crate::redact::redact(message, &self.secrets));
            }
            radian_session::RuntimeEvent::TurnStarted { .. } => {
                *self.error.lock().unwrap() = None;
            }
            _ => {}
        }
    }
}

/// Builds the event sink shared by the session and the prompt driver.
pub(crate) fn events(
    out: Outbox,
    session_id: String,
    workspace: PathBuf,
    window: Option<u64>,
) -> Arc<AcpEvents> {
    Arc::new(AcpEvents::new(out, session_id, workspace, window))
}
