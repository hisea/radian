//! A minimal, embeddable agent runtime.
//!
//! An [`Agent`] holds an injected [`Model`], [`ToolProvider`] and [`Hooks`]. Each
//! conversation is an [`AgentSession`]; each user turn is an [`ActiveRun`] driven with
//! [`ActiveRun::step`] or [`ActiveRun::run`].

mod agent;
mod event;
mod hooks;
mod journal;
mod message;
mod model;
mod outcome;
mod preflight;
mod steering;
mod tool;
mod turn;

pub use agent::{Agent, AgentConfig, AgentSession, DEFAULT_MAX_MODEL_CALLS, ReplaceError};
pub use budget::{
    BudgetReport, BudgetStatus, ContextBudget, DEFAULT_BYTES_PER_TOKEN, RequestSize, UsageSource,
    estimate_tokens, over_budget,
};
pub use compaction::{
    DEFAULT_SUMMARY_MAX_TOKENS, MAX_KEEP_TARGET, MIN_KEEP_TARGET, Plan, PlanError,
    SHORTEN_TARGET_PERCENT, SUMMARY_MESSAGE_PREFIX, SUMMARY_SYSTEM_PROMPT, SummaryError,
    TOOL_RESULT_LIMIT_CHARS, check_summary, compacted_history, keep_target, message_tokens, plan,
    plan_with_keep, shorten_request, shorten_target, summary_request, summary_system_prompt, units,
};
pub use event::{AgentEvent, CompactionTrigger, EventSink, NoopSink};
pub use hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
pub use journal::{
    JournalError, JournalRecord, ReplacementEnd, ReplayError, TranscriptFold, TranscriptJournal,
    UncommittedReplacement, recover, repair, replay,
};
pub use message::{
    ContentPart, InputSource, Message, MessageId, MessageMeta, OpaqueJson, ToolCall, ToolResult,
};
pub use model::{
    CONTEXT_OVERFLOW_PREFIX, Model, ModelContext, ModelError, ModelRequest, ModelResponse,
    ModelUsage, StopReason,
};
pub use outcome::{
    CancelReason, CompactionError, CompactionFailure, CompactionOutcome, IncompleteReason,
    ResumeState, RunError, RunOutcome, RunStatus, SessionError, Step,
};
pub mod budget;
pub mod compaction;
pub use steering::{SteerError, SteerMode, SteeringHandle};
pub use tool::{
    NoTools, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider,
    ToolSession, ToolSpec,
};
pub use turn::{
    ActiveRun, CancellationHandle, DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_TOOL_CANCEL_GRACE,
    TurnOptions,
};

#[cfg(feature = "testing")]
pub mod testing;

pub use tokio_util::sync::CancellationToken;
