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
///
/// `#[non_exhaustive]`: new terminal reasons are expected (the plan adds more), and a
/// downstream exhaustive match must not break when one lands. Match with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
// `RunOutcome` carries a transcript. Boxing it would make the public `Step::Finished`
// shape less ergonomic solely because host metadata can contain an opaque JSON value.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Continue,
    Finished(RunOutcome),
}

/// Why automatic compaction could not proceed (ADR 0018). Any of these fails the turn
/// without sending the oversized request; none of them is fatal to the session (ADR 0017).
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    /// The compacted request is not small enough (or not smaller at all). The compaction is
    /// not committed. `target` is the ceiling it had to reach: the input budget `B` for the
    /// proactive path, `reference/2` for the overflow fallback, or `before` for a manual
    /// `/compact` (which only has to shrink it).
    #[error(
        "compaction would not shrink the request enough: {before} -> {after} tokens against a \
         target of {target}"
    )]
    Insufficient {
        before: u64,
        after: u64,
        target: u64,
    },
    /// Even a summary plus the smallest keepable unit does not fit the input budget.
    #[error("nothing fits the input budget even after compaction")]
    NothingFits,
    /// The summarizer returned text that is empty or over the summary length ceiling, twice.
    #[error("the summary was invalid: {0}")]
    InvalidSummary(String),
    /// A previous compaction this turn was below the 75% target, so the turn must not
    /// compact again (ADR 0018).
    #[error("the previous compaction this turn was below target, so it will not compact again")]
    BelowTarget,
    /// The transcript changed between planning and commit, so the candidate view is stale
    /// (ADR 0018 §revision 与并发). Nothing is committed.
    #[error("the transcript changed while the summary was being made")]
    SourceChanged,
    /// The summarization model call itself failed.
    #[error("the summarizer failed: {0}")]
    Summary(#[from] ModelError),
    /// The compaction was cancelled (Ctrl-C/Esc, or the turn deadline). Nothing was
    /// committed; a manual `/compact` reports this instead of a hard failure.
    #[error("the compaction was cancelled")]
    Cancelled,
}

/// Why a compaction run failed (ADR 0018). The automatic paths fold this into `RunError`; a
/// manual `/compact` reports it to its caller. `Journal` is fatal to the session (the file and
/// memory may differ); the others leave the session usable.
#[derive(Debug, thiserror::Error)]
pub enum CompactionFailure {
    #[error("journal: {0}")]
    Journal(#[from] JournalError),
    #[error("hook {point:?} failed: {error}")]
    Hook { point: HookPoint, error: HookError },
    #[error(transparent)]
    Compaction(#[from] CompactionError),
}

/// What a committed compaction (or a no-op) produced (ADR 0018). Returned by a manual
/// `/compact`; the automatic paths use the same result internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionOutcome {
    /// The transcript was replaced. `tokens_before`/`tokens_after` are the projections the
    /// gain rule used.
    Compacted {
        revision: u64,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// The history already fits the keep target, so there was nothing worth summarizing.
    NothingToCompact,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Compaction(#[from] CompactionError),
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
