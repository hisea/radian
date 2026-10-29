//! The `PermissionRequest` and `Notification` seams: a host's command hooks answer the
//! question the rules asked, and observe a prompt about to reach a human.
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

/// The `Notification` `permission_prompt` value: radian is about to ask a human for
/// approval (the TUI modal, `exec`'s terminal prompt).
pub const NOTIFICATION_PERMISSION_PROMPT: &str = "permission_prompt";
/// The `Notification` `idle_prompt` value: a TUI turn ended and no input arrived for the
/// idle period.
pub const NOTIFICATION_IDLE_PROMPT: &str = "idle_prompt";
/// The closed set of `Notification` types, one declaration for the parser, the manual's
/// drift test and the two hosts.
pub const NOTIFICATION_TYPES: &[&str] = &[NOTIFICATION_PERMISSION_PROMPT, NOTIFICATION_IDLE_PROMPT];

/// A host's `Notification` hook chain (ADR 0045 §6). It is called at the moment radian is
/// about to ask a human, and when the TUI's idle timer fires. It observes only: an
/// implementation must start its work and return at once, so a slow or hanging hook can
/// never delay the prompt or the frame. `tool` is the tool name for `permission_prompt`
/// and `None` for `idle_prompt`; the message never carries arguments or transcript text.
pub trait NotificationHooks: Send + Sync {
    fn notification(&self, notification_type: &str, tool: Option<&str>);
}
