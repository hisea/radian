//! The permission layer (ADR 0012): `ApprovalPolicy` + `ToolRuleSet`, as one
//! `agent_core::Hooks`.
//!
//! `SandboxMode` (the execution boundary) is a separate layer and is not widened here: this
//! crate only decides `allow` / `ask` / `deny`.
//!
//! # What this is, and what it is not
//!
//! - Rules are `(tool matcher, resource matcher) -> allow | ask | deny`. Priority is
//!   `deny` > `ask` > `allow`; **no rule matching means `ask`**.
//! - A command `allow` rule only matches a **simple command** — no `; & | \` $ ( ) > <`,
//!   quotes, globs or newline. Any of those keeps the call at `ask`. A command `deny` rule is
//!   matched conservatively over every separator-split segment, so any segment hit denies the
//!   whole command. See [`shell`] for the exact, deliberately narrow rules and their limits.
//! - Path rules match the **resolved** path (symlinks and `..` removed), so they cannot be
//!   escaped by either.
//! - **This is not a shell parser and does not claim to be one.** Aliases, `eval`, a called
//!   program that starts its own shell, and quoted nested commands are invisible here. The
//!   real execution boundary is the sandbox; this layer asks/denies on top of it.
//! - "Allow for this session" is **real-time state**: it applies to the next call in the same
//!   session (no turn-boundary wait), is never written into a profile, and is not restored on
//!   resume (ADR 0004, ADR 0006 §5.9).
//! - A turn whose source is not `User` cannot turn an `ask` into an `allow`, and cannot borrow
//!   a session grant. `HookContext` has no source field, so the host sets the current turn's
//!   source with [`PermissionHooks::set_turn_source`]; an unset source is treated as
//!   non-human (fail safe).

pub mod approval;
pub mod audit;
pub mod profile;
pub mod readonly;
pub mod rule;
pub mod shell;

mod hooks;

pub use approval::{ApprovalError, ApprovalOutcome, ApprovalRequest, Approver};
pub use audit::{
    Approver as AuditApprover, AuditSink, NoopAudit, PermissionResolved, RecordingAudit,
    ResolvedDecision,
};
pub use hooks::{PermissionHooks, ToolGate, policy_change_event, runtime_event};
pub use profile::{ApprovalPolicy, Preset, Profile, ProfileError};
pub use rule::{
    Action, Evaluated, Resource, ResourceMatcher, Rule, ToolMatcher, canonical_tool_name, evaluate,
    resource_of,
};
