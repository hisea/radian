use crate::hooks::{HookError, HookPoint};
use crate::journal::JournalError;
use crate::message::Message;
use crate::model::ModelError;
use crate::tool::ToolError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncompleteReason {
    MaxTokens,
    ContentFiltered,
    /// The model said it wanted tools but sent no calls.
    ToolUseWithoutCalls,
    UnknownStopReason(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    Requested,
    DeadlineExceeded,
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStatus {
    Completed,
    Incomplete(IncompleteReason),
    Refused,
    BudgetExceeded,
    Cancelled(CancelReason),
    /// A model error or a model hook error; `step()` returned the `RunError`.
    Failed,
    /// An in-flight `step()`/`run()` future was dropped. The session cannot continue;
    /// use `AgentSession::history_for_resume`.
    Poisoned,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunOutcome {
    pub status: RunStatus,
    /// The last assistant message of this turn, if any.
    pub final_message: Option<Message>,
    /// Steering messages that arrived but were never written to the transcript.
    pub unapplied_steering: Vec<Message>,
    pub model_calls: u32,
}

/// Result of one `step()`.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Continue,
    Finished(RunOutcome),
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("hook {point:?} failed: {error}")]
    Hook { point: HookPoint, error: HookError },
    #[error("journal: {0}")]
    Journal(#[from] JournalError),
    #[error("the turn has already finished")]
    Finished,
    #[error("the session is poisoned; resume from history_for_resume()")]
    Poisoned,
    #[error(transparent)]
    Session(#[from] SessionError),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("invalid history: {0}")]
    InvalidHistory(String),
    #[error("a turn input must be a user message")]
    NotUserMessage,
    #[error("the session is poisoned; resume from history_for_resume()")]
    Poisoned,
    #[error("the session is closed")]
    Closed,
    /// A journal write failed, so memory and journal may disagree (ADR 0001, D6). Rebuild the
    /// session from the journal. `history_for_resume()` is **not** enough: its history can end
    /// in tool calls without results, which `open_session_with_history` rejects.
    #[error("the session needs recovery: a journal write failed; rebuild it from the journal")]
    RecoveryRequired,
    #[error("tool session: {0}")]
    Tools(#[from] ToolError),
    #[error("tool {tool}: {message}")]
    ToolSpec { tool: String, message: String },
}

/// What a host needs to continue a conversation in a new session.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeState {
    /// The transcript. After a poisoned turn every tool call is paired with a result. After a
    /// journal failure (`SessionError::RecoveryRequired`) the tail may hold calls without
    /// results, and the history must be repaired before it can open a session.
    pub history: Vec<Message>,
    /// Steering messages from turns that ended without returning an outcome.
    pub unapplied_steering: Vec<Message>,
}
