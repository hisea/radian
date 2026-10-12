use serde_json::Value;

use crate::hooks::HookPoint;
use crate::message::Message;
use crate::model::{ModelUsage, StopReason};
use crate::outcome::RunStatus;

/// Observations of a turn. Events never change control flow; use `Hooks` for that.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AgentEvent {
    TurnStarted,
    SteeringApplied {
        count: usize,
    },
    ModelRequestStarted {
        model_call: u32,
    },
    /// Emitted by model adapters while streaming.
    ModelTextDelta {
        text: String,
    },
    ModelResponseReceived {
        stop_reason: StopReason,
        tool_calls: usize,
        /// The provider's usage for this response; `None` when it reported none.
        usage: Option<ModelUsage>,
    },
    /// Emitted in the order calls actually start; `index` is the call's position in its batch.
    ToolStarted {
        index: usize,
        call_id: String,
        name: String,
    },
    /// Emitted after a transcript modification was journaled and applied to memory; never
    /// emitted for a modification whose journal append failed.
    TranscriptAppended {
        index: usize,
        message: Message,
    },
    /// Emitted after a complete transcript replacement has been journaled and applied. The
    /// replacement's individual messages are journaled but do not each emit
    /// `TranscriptAppended`, to avoid flooding the host.
    TranscriptReplaced {
        revision: u64,
        len: usize,
    },
    /// Emitted by tool sessions.
    ToolProgress {
        index: usize,
        progress: Value,
    },
    /// Emitted in completion order, which may differ from the transcript order.
    ToolFinished {
        index: usize,
        call_id: String,
        is_error: bool,
    },
    HookFailed {
        hook: HookPoint,
        message: String,
    },
    /// Emitted exactly once per turn.
    RunFinished {
        status: RunStatus,
    },
}

/// Receives events synchronously from the turn loop. Must be short and non-blocking; forward
/// to a queue if the host needs to do more. A bounded queue may drop progress events but must
/// keep `RunFinished`.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

impl<F: Fn(AgentEvent) + Send + Sync> EventSink for F {
    fn emit(&self, event: AgentEvent) {
        self(event)
    }
}

/// Discards all events.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: AgentEvent) {}
}
