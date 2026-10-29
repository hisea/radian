//! The permission layer (ADR 0012): `ApprovalPolicy` + `ToolRuleSet`, as one
//! `radian_core::Hooks`.
//!
//! `SandboxMode` (the execution boundary) is a separate layer and is not widened here: this
//! crate only decides `allow` / `ask` / `deny`.
//!
//! # What this is, and what it is not
//!
//! - Rules are `(tool matcher, resource matcher) -> allow | ask | deny`. Priority is
//!   `deny` > `ask` > `allow`; **no rule matching means `ask`**.
//! - A command `allow` rule only matches a **simple command** — no `; & |` `` ` `` `$ ( ) > <`,
//!   quotes, globs or newline; a wrapper first token (`xargs`, `sh`, …) never matches
//!   either. Any of those keeps the call at `ask`, unless the command splits into
//!   `&&`/`||`/`;`/`|`-separated simple segments that are **all** allowed (a user rule
//!   or the read-only table per segment, with `cd` and wrapper restrictions). A command
//!   `deny` rule is matched conservatively over every separator-split segment, so any
//!   segment hit denies the whole command. See [`shell`] for the exact, deliberately narrow rules and their limits.
//! - Path rules match the **resolved** path (symlinks and `..` removed), so they cannot be
//!   escaped by either.
//! - **This is not a shell parser and does not claim to be one.** Aliases, `eval`, a called
//!   program that starts its own shell, and quoted nested commands are invisible here. The
//!   real execution boundary is the sandbox; this layer asks/denies on top of it.
//! - "Allow for this session" is **real-time state**: it applies to the next call in the same
//!   session (no turn-boundary wait), is never written into a profile, and is not restored on
//!   resume (ADR 0004, ADR 0006 §5.9).
//! - A turn whose source is not `User` cannot turn an `ask` into an `allow`, and cannot borrow
//!   a session grant; an `ask` from such a turn is denied unless a host-installed
//!   [`PermissionRequestHooks`] chain answers it. `HookContext` has no source field, so the host
//!   sets the current turn's source with [`PermissionHooks::set_turn_source`]; an unset source
//!   is treated as non-human (fail safe).
//! - A host may install a [`PermissionRequestHooks`] chain (ADR 0045 §4). It runs on every
//!   `ask` no session grant covers, before the approver and before the non-human denial, so a
//!   script can answer a question no human will see. It answers only the question: a `deny`
//!   rule and the sandbox stay untouched.

pub mod approval;
pub mod audit;
pub mod profile;
pub mod readonly;
pub mod request;
pub mod rule;
pub mod shell;

mod hooks;

pub use approval::{ApprovalError, ApprovalOutcome, ApprovalRequest, Approver};
pub use audit::{
    Approver as AuditApprover, AuditSink, NoopAudit, PermissionResolved, RecordingAudit,
    ResolvedDecision,
};
pub use hooks::{PermissionHooks, ToolGate, policy_change_event, runtime_event};
pub use profile::{
    APPROVAL_POLICIES, ApprovalPolicy, Preset, Profile, ProfileError, TIER_ALIASES, TIER_NAMES,
};
pub use request::{
    NOTIFICATION_IDLE_PROMPT, NOTIFICATION_PERMISSION_PROMPT, NOTIFICATION_TYPES,
    NotificationHooks, PermissionRequestDecision, PermissionRequestHooks,
};
pub use rule::{
    Action, Evaluated, Resource, ResourceMatcher, Rule, ToolMatcher, evaluate, resource_of,
};
