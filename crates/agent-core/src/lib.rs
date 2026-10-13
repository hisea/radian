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
pub use event::{AgentEvent, EventSink, NoopSink};
pub use hooks::{HookContext, HookError, HookPoint, Hooks, ToolDecision};
pub use journal::{
    JournalError, JournalRecord, ReplayError, TranscriptJournal, recover, repair, replay,
};
pub use message::{
    ContentPart, InputSource, Message, MessageId, MessageMeta, OpaqueJson, ToolCall, ToolResult,
};
pub use model::{
    Model, ModelContext, ModelError, ModelRequest, ModelResponse, ModelUsage, StopReason,
};
pub use outcome::{
    CancelReason, IncompleteReason, ResumeState, RunError, RunOutcome, RunStatus, SessionError,
    Step,
};
pub mod budget;
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
