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
    estimate_tokens, estimate_tokens_with, over_budget,
};
pub use compaction::{
    DEFAULT_SUMMARY_MAX_TOKENS, FIRST_USER_MESSAGE_MAX, MAX_KEEP_TARGET, MAX_USER_MESSAGE_BUDGET,
    MIN_KEEP_TARGET, MIN_USER_MESSAGE_BUDGET, Plan, PlanError, RetainedUserMessage,
    SHORTEN_TARGET_PERCENT, SUMMARY_FACTS_DELETE_OLDEST, SUMMARY_FACTS_ORDER_RULE,
    SUMMARY_MESSAGE_PREFIX, SUMMARY_PLANS_ARE_NOT_FACTS, SUMMARY_SYSTEM_PROMPT, SummaryError,
    TOOL_RESULT_LIMIT_CHARS, USER_MESSAGES_HEADING, check_summary, compacted_history,
    compaction_summary_text, compaction_user_messages, deterministic_summary,
    deterministic_summary_line, is_compaction_summary, keep_target, message_tokens, omitted_marker,
    plan, plan_with_keep, select_user_messages, shorten_request, shorten_target,
    summary_max_tokens, summary_request, summary_system_prompt, truncate_kept_tool_results,
    truncate_tool_result_text, units, user_message_budget, verified_facts_max_tokens,
};
pub use event::{AgentEvent, CompactionTrigger, EventSink, NoopSink};
pub use hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
pub use journal::{
    JournalError, JournalRecord, ReplacementEnd, ReplayError, TranscriptFold, TranscriptJournal,
    UncommittedReplacement, recover, repair, replay,
};
pub use message::{
    ATTACHMENT_KIND, ContentPart, InputSource, Message, MessageId, MessageMeta, OpaqueJson,
    ToolCall, ToolResult,
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
