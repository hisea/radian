use std::time::Duration;

use serde_json::Value;

use crate::budget::BudgetReport;
use crate::hooks::HookPoint;
use crate::message::Message;
use crate::model::{ModelUsage, StopReason};
use crate::outcome::RunStatus;

/// Why a compaction was requested (ADR 0018). A host uses it to phrase its `[compaction]`
/// line: `budget` is the proactive pre-request compaction, `overflow` is the provider-overflow
/// fallback, and `manual` is a user `/compact`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    Budget,
    Overflow,
    Manual,
}

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
        /// The context-budget projection for this request (ADR 0018). Over `B` the request is
        /// still sent; a host's status bar shows `projected_tokens / input_budget_tokens`.
        /// `None` when no budget is configured.
        budget: Option<BudgetReport>,
    },
    /// The core is about to retry a transient model failure in place (ADR 0017).
    /// Nothing of the failed attempt was committed; a host should clear any streaming block it
    /// was showing for this call (the failed attempt's deltas are live-only, ADR 0010) and may
    /// show `retrying {attempt}/{max} in {delay}: {reason}`. Emitted once per retry, before the
    /// wait, so a cancellation during that wait is announced by the event already sent.
    ModelRetrying {
        /// 1 for the first retry, up to `max`.
        attempt: u32,
        /// The configured retry cap (`AgentConfig::retries`).
        max: u32,
        /// How long the core will wait before the next attempt.
        delay: Duration,
        /// The failed attempt's error text, masked by the adapter.
        reason: String,
    },
    /// Emitted by model adapters while streaming.
    ModelTextDelta {
        text: String,
    },
    /// Emitted by model adapters while streaming, for a provider's separate reasoning
    /// channel (e.g. OpenAI-compatible `delta.reasoning_content`). It rides the same event
    /// outlet as [`AgentEvent::ModelTextDelta`] but is a distinct kind, so a host can render
    /// it apart from the visible answer. Like the text delta it is a live projection (ADR
    /// 0010): dropping or merging it is allowed, and only the committed message's
    /// `reasoning` field is durable.
    ModelReasoningDelta {
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
    /// Compaction is about to summarize (ADR 0018). `tokens_before` is the projection that
    /// triggered it; `keep_target` is the planned kept suffix; `trigger` says whether the
    /// budget, a provider overflow, or a user `/compact` asked for it.
    CompactionStarted {
        revision: u64,
        tokens_before: u64,
        keep_target: u64,
        trigger: CompactionTrigger,
    },
    /// The compaction committed and the request is being rebuilt. `below_target` says the
    /// compacted view fit the budget but missed the 75% target, so the rest of this turn will
    /// not compact again.
    ///
    /// ADR 0018, decision 3: `deterministic` is `true` when the summary path failed
    /// and this is the deterministic fallback (no model call, so no `CompactionUsage`), and
    /// `dropped` is then the number of transcript messages the fallback left out of the new
    /// view: the whole summarized prefix the plan removed, minus the previous summary message
    /// the new summary replaces, minus the user messages the verbatim list still carries. A
    /// model summary sends `deterministic: false, dropped: 0`. The host wording names the
    /// count — "compacted without a summary (N dropped)".
    CompactionFinished {
        revision: u64,
        tokens_before: u64,
        tokens_after: u64,
        below_target: bool,
        deterministic: bool,
        dropped: u64,
    },
    /// One **summarization** model call reported its usage (ADR 0018: the real call
    /// is billed whether the compaction commits or not, and a retry counts too).
    ///
    /// Deliberately not [`AgentEvent::ModelResponseReceived`]: that event is a conversation
    /// response, which a host timestamps and times as this turn's generation. The summarizer's
    /// call is neither — it runs between turns and its response never reaches the transcript —
    /// but it does cost money, so its usage must reach a host that shows a session total.
    /// Emitted once per attempt, **after** the journal record of the same call, so a host that
    /// sees the event has the durable record too.
    CompactionUsage {
        revision: u64,
        /// The summarizer the call used (ADR 0018's `CompactionModel`).
        model: String,
        usage: ModelUsage,
    },
    /// The compaction failed; the turn ends `failed` without sending the oversized request.
    /// `reason` is the `Display` of the `CompactionError`.
    CompactionFailed {
        reason: String,
    },
    /// The projection is over `B` but the history already fits the keep target, so there is
    /// nothing worth summarizing. The request is sent as it is.
    CompactionSkipped {
        reason: String,
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
