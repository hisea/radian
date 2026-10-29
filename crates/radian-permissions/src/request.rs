//! The `PermissionRequest` seam: a host's command hooks answer the question the rules asked.
//!
//! The layer itself owns the position — after the rules and any session grant produced
//! `ask`, before the approver and before the non-human-source denial — and the shape of an
//! answer. It knows nothing about processes, config files or `radian-host`: a host installs
//! one [`PermissionRequestHooks`] on [`crate::PermissionHooks::set_permission_request_hooks`]
//! and keeps the two crates independent (ADR 0045 §4).

use futures_util::future::BoxFuture;

use radian_core::{HookError, ToolCall};

/// What the hook chain decided for one call the rules judged `ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestDecision {
    /// No hook gave a decision: the layer takes its ordinary path (the approver on a human
    /// turn, the denial on a non-human turn or a terminal-less `radian exec`).
    NoDecision,
    /// A hook said `allow`: the call runs without asking anyone. It answers only this
    /// question; `deny` rules and the sandbox are untouched. `session_grant` is a hook's
    /// `updatedPermissions` `addRules`/`session`/`allow` item: this call also records the
    /// same session grant "allow for this session" would (exact key, or a shell prefix),
    /// so later identical calls never ask or run the hook again (ADR 0045 §4).
    Allow { session_grant: bool },
    /// A hook said `deny` (or exited 2). `reason` is the text of the denied tool result;
    /// `None` means the hook gave no message and the layer supplies the default.
    Deny { reason: Option<String> },
}

/// One host's `PermissionRequest` hook chain. `Err` is a hook error: the layer fails closed
/// (Core denies the call with `hook_error`) and never falls back to asking a human
/// (ADR 0017 §4).
///
/// Boxed like [`crate::Approver`] so the trait stays dyn-compatible. If the returned future
/// is dropped (the turn was cancelled) the implementation is responsible for stopping its
/// own work, including killing a child process group.
pub trait PermissionRequestHooks: Send + Sync {
    fn permission_request<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> BoxFuture<'a, Result<PermissionRequestDecision, HookError>>;
}
