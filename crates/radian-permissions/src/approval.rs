//! The approver seam: how the layer asks a human, and how that ask is answered.

use futures_util::future::BoxFuture;

/// What the human is shown. `reason` is the rule's reason when an `ask` rule matched.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub call_id: String,
    pub tool: String,
    /// The resource as the call wrote it: a raw `path`, a `command`, or the tool name.
    pub resource: String,
    /// The resource the rules actually matched. For a path this is the **resolved** one
    /// (relative to the profile workspace, with `.`/`..` removed and existing symlinks
    /// resolved); for a command or a name it is the same as `resource`. A prompt must show
    /// this one: otherwise `a/../secret` would read as if it stayed under `a/`.
    pub resolved_resource: String,
    /// The call's arguments, for the prompt to display. Opaque to this layer.
    pub arguments: serde_json::Value,
    pub reason: Option<String>,
}

/// The human's answer. `AllowForSession` is a real-time grant (ADR 0004): it applies to the
/// next call, is never written into a profile, and is not restored on resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    AllowOnce,
    AllowForSession,
    Deny { reason: Option<String> },
}

/// The approver itself failed. The hook turns this into `HookError`, which Core treats as a
/// denial (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ApprovalError(pub String);

/// One host-implemented approval channel (CLI prompt, TUI dialog, remote). The future must
/// return when the answer arrives; the permission layer races it against
/// `HookContext.cancellation`, so a wait that never ends is interrupted, not hung.
///
/// `decide` returns a boxed future so the trait is dyn-compatible and a host can inject an
/// `Arc<dyn Approver>` (the same shape as `agent_core::TranscriptJournal`).
pub trait Approver: Send + Sync {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>>;
}
