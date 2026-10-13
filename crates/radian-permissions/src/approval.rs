//! The approver seam: how the layer asks a human, and how that ask is answered.

use futures_util::future::BoxFuture;

/// What the human is shown. `reason` is the rule's reason when an `ask` rule matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub request_id: String,
    pub call_id: String,
    pub tool: String,
    pub resource: String,
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
