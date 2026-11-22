//! User-level external command hooks (ADR 0045 slice A): the Claude/Codex subset as an
//! [`radian_core::Hooks`] implementation.
//!
//! Six events, all shell commands from the user's own `config.toml` (`[hooks]`): the four
//! deciding events below and the two observe-only ones (ADR 0045 §6) documented at the end:
//!
//! - `PreToolUse` runs before each matching tool call. Exit 0 continues (JSON stdout may
//!   carry `hookSpecificOutput.permissionDecision: "deny"` or `updatedInput`); exit 2
//!   denies the call with the hook's stderr as the reason; any other exit or a timeout is
//!   a hook error and fails closed (ADR 0017: the call is denied with `hook_error`).
//!   `"allow"`/`"ask"` decisions are refused with a warning and treated as no decision —
//!   a hook can only narrow what the permission rules already allowed (ADR 0012 P1).
//!   `updatedInput` becomes [`ToolDecision::Rewrite`]: re-validated, hooks not re-run.
//! - `PermissionRequest` runs inside the permission layer's `ask` branch (ADR 0045 §4),
//!   after the rules, the policy and any session grant produced `ask`, before the approver
//!   and before the non-human-source denial, so a script answers a prompt no human will
//!   see. Exit 0 may print `hookSpecificOutput.decision.behavior` `"allow"` or `"deny"`
//!   (with an optional `message`); no decision leaves today's path; exit 2 denies with
//!   stderr; any other exit, timeout, or exit-0 stdout that is not JSON fails closed as
//!   `hook_error` and never falls back to a prompt. `updatedPermissions` may carry one
//!   `{"type":"addRules","destination":"session","behavior":"allow"}` item, which records
//!   the session grant "allow for this session" would; other items warn once and record
//!   nothing. `updatedInput`/`interrupt` warn once and are ignored.
//! - `PostToolUse` runs after each matching call's result, before it is committed. Exit 0 may
//!   carry `hookSpecificOutput.additionalContext` (non-error feedback appended to the
//!   result), `decision: "block"` with `reason` (the JSON form of exit 2: the feedback
//!   rides along and the result is marked an error), and `hookSpecificOutput.updatedToolOutput`
//!   (a string that replaces the result's content). Exit 2 feeds the hook's stderr back to the
//!   model alongside the result; any other exit or a timeout is a hook error and fails closed
//!   (the result is replaced with `hook_error`).
//!   A hook with `async = true` starts the same way and the result continues at once
//!   (ADR 0045 §5): its exit-0 `additionalContext` enters context through `inject` before the
//!   next model call, and an `asyncRewake` exit 2 wakes an idle session through the wake gate
//!   (failing that, it degrades to an inject). `permissionDecision`, `updatedInput`,
//!   `updatedToolOutput` and `decision` are ignored with one warning: the call already ran.
//! - `Stop` runs once per completed turn (see [`Hooks::on_turn_end`]: only
//!   [`RunStatus::Completed`] runs the hook; any other terminal status runs nothing).
//!   Exit 2 starts another turn with the hook's stderr as a `System` `followup` through
//!   the inbox; `decision: "block"` and `hookSpecificOutput.additionalContext` join the same
//!   chain. Continuations are limited to a [`WakeGate`] window's worth; past the gate they
//!   degrade to a marked `inject` (no wake) with the wake-budget notice, the same degrade as
//!   the background-job notifier (ADR 0045 §2). An `async` `Stop` hook is not part of that
//!   chain: it is spawned and its own result is delivered afterwards (ADR 0045 §5).
//! - `StopFailure` (once per turn that ends `Failed` on a model error) and `Notification`
//!   (`permission_prompt` when a human is about to be asked, `idle_prompt` from the TUI's
//!   60 s timer) only observe (ADR 0045 §6): they start in the background, their stdout is
//!   not read, and their exit code or timeout only adds a recent-runs entry. Their matchers
//!   are closed sets, not regexes; `async`/`asyncRewake` on them are accepted and inert.
//!   `exec -p` waits at most 5 s for a `StopFailure` run, then kills its process group.
//!
//! Async hooks are not jobs: they never enter the [`Jobs`] registry, `/jobs` or the `job`
//! tool. The `/hooks` page shows them under "Running", `Stop` lists each one in
//! `background_tasks` with `type: "hook"`, and the intent-stop validator treats one like a
//! running job (no trigger while any is in flight). Their lifecycle (ADR 0045 §5): a hook is
//! identified by its event plus its `command` and `args`, and a new run of a hook whose
//! earlier run is still running kills that run's process group (it delivers nothing and its
//! record says `cancelled`); at most [`MAX_ASYNC_HOOKS`] processes run per session (a run
//! whose process exited while its result is still delivering holds no slot), and a run over
//! the cap does not start (one warning, a `failed to start` record); a cancelled turn leaves
//! running async hooks alone; the session's end ([`CommandHooks::end_session`], called when
//! the driver is joined: the TUI exiting, `/new`, an `exec` run ending) kills every running
//! one and delivers nothing, and `exec -p` never waits for them. The end is one critical
//! section with delivery: a result either finishes its inbox enqueue first or finds the
//! session ended, and nothing reaches the old session after the end returns.
//!
//! Two layers, one chain (ADR 0045 slice B): the project hooks
//! (`<workspace>/.radian/hooks.json`, loaded only when trusted) run first, then the user's
//! own `[hooks]` — so the user's hooks see the final arguments a project `updatedInput`
//! may have rewritten (Core re-validates a rewrite but never re-runs hooks, ADR 0012 P3).
//! The project layer can never turn a user hook off. Within one event a hook identical to
//! one already run — the same `command` plus the same `args` — is skipped (one run even when
//! both layers name it); a deny
//! from any layer stops the rest; two `Stop` exit-2 verdicts open a single continuation
//! with the reasons joined in one message, counted as one wake.
//!
//! The matcher is Claude's case-sensitive pattern over **Radian** tool names: `*` or a
//! missing/empty matcher matches every tool, otherwise the pattern is a regular
//! expression matched against the whole tool name (`^(?:<matcher>)$`, so `Edit|Write`
//! and `Notebook.*` work and `sh` does not match `shell`). An invalid pattern is skipped
//! with one warning at load time and never runs. `Stop` ignores it. There is no runtime
//! aliasing of Claude's tool names.
//!
//! Hook commands run inside the same OS sandbox the `shell` tool uses when the session has
//! one (enforced or not, with the same environment rules as `shell`); with no sandbox in
//! the slot they run as plain `/bin/sh -c` with the host environment. A hook with `args`
//! starts `command` directly with exactly those arguments and never through a shell
//! (ADR 0045 §4). Every run leaves one in-memory record, newest kept (the manual's "Recent
//! runs"), shown on the `/hooks` page and logged at info.

mod compile;
mod events;
mod exec;
mod observe;
mod output;
mod process;
mod stop;
#[cfg(test)]
mod tests;
mod trait_impls;

use self::compile::*;
use self::output::*;
use self::process::*;

use crate::config::{
    HookCommand, HookMatcher, HooksConfig, NOTIFICATION_IDLE_PROMPT,
    NOTIFICATION_PERMISSION_PROMPT, NOTIFICATION_TYPES, STOP_FAILURE_MATCHERS,
};
use crate::goal::SharedGoal;
use crate::intent_stop;
use crate::wake_gate::WakeGate;
use radian_coding::Jobs;
use radian_core::{
    ContentPart, HookContext, HookError, Hooks, InputSource, Message, MessageId, ModelError,
    RunOutcome, RunStatus, ToolCall, ToolDecision, ToolResult,
};
use radian_permissions::{
    NotificationHooks, PermissionHooks, PermissionRequestDecision, PermissionRequestHooks,
};
use radian_runtime::{Inbox, InboxEntry};
use radian_sandbox::SandboxSlot;
use serde::Deserialize;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::Notify;

/// How many characters of a hook's feedback enter the transcript (deny reasons, PostToolUse
/// feedback, `additionalContext`, `decision.reason`, `updatedToolOutput`, Stop
/// continuations). Hooks are the user's own commands, but their output is model context, so
/// it stays bounded.
pub const REASON_CHAR_LIMIT: usize = 2000;

/// How many validator-fired continuations may follow each other: the third consecutive
/// intent-only turn end reports `blocked` instead of opening another turn (backlog
/// `turn-end-continuation.md` §B.3).
pub const MAX_INTENT_STOP_CONSECUTIVE: u32 = 2;

/// How many hook-opened continuations may follow each other while their turns do no work:
/// the continuation that would be the 8th in a row (a hook exit 2, `decision: "block"`,
/// or `additionalContext`) degrades instead of opening a turn (ADR 0045 §3). A continued
/// turn that called a tool resets the streak, so real work never counts like an idle
/// continuation.
pub const MAX_HOOK_IDLE_CONTINUATIONS: u32 = 8;

/// The TUI notice line for the consecutive idle hook-continuation cap: no further hook
/// continuation opens, herdr reports `blocked` with the `hook stop` label, and sending any
/// message continues.
pub fn hook_stop_cap_line() -> String {
    format!(
        "hook stop: {MAX_HOOK_IDLE_CONTINUATIONS} hook continuations in a row ended with no \
         tool call — send any message to continue"
    )
}

/// What opened the turn now running (ADR 0045 §2): nothing, the built-in intent-stop
/// validator, or a user hook continuation (exit 2, `decision: "block"`,
/// `additionalContext`). The hook idle-continuation counter only counts the last kind;
/// the reset after real work covers every continuation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Continuation {
    #[default]
    None,
    Validator,
    Hook,
}

impl Continuation {
    fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Validator => 1,
            Self::Hook => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Validator,
            2 => Self::Hook,
            _ => Self::None,
        }
    }

    /// Whether any `Stop` continuation opened this turn (`stop_hook_active`).
    fn is_continuation(self) -> bool {
        self != Self::None
    }
}

/// The warning channel: the host routes it the way it routes its other startup/runtime
/// lines (the CLI prints it, the TUI shows it as a notice).
pub type HooksWarning = Arc<dyn Fn(&str) + Send + Sync>;

/// How many finished hook runs one session keeps in memory, newest kept. The `/hooks` page
/// shows them; they are never journaled (ADR 0001 records transcript events only).
pub const HOOK_RUN_HISTORY: usize = 50;

/// The most async hook processes one session runs at once (ADR 0045 §5). A run that would be
/// the ninth does not start: one warning line and a `failed to start` recent-runs entry.
pub const MAX_ASYNC_HOOKS: usize = 8;

/// The closed set of ways one hook run ends, as the record, the `/hooks` page and the docs
/// drift test spell them (the manual's "Recent runs"). `status` carries the exit code; the
/// other three stand alone.
pub const HOOK_RUN_END_STATUS: &str = "status";
pub const HOOK_RUN_END_TIMED_OUT: &str = "timed out";
pub const HOOK_RUN_END_CANCELLED: &str = "cancelled";
pub const HOOK_RUN_END_FAILED_TO_START: &str = "failed to start";
pub const HOOK_RUN_ENDS: &[&str] = &[
    HOOK_RUN_END_STATUS,
    HOOK_RUN_END_TIMED_OUT,
    HOOK_RUN_END_CANCELLED,
    HOOK_RUN_END_FAILED_TO_START,
];

/// How one hook run ended: the exit code, the timeout, a cancellation (the process was
/// killed by a signal, or the run could not be reaped), or a command that never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRunEnd {
    /// The process exited with this code.
    Status(i32),
    /// The run passed its timeout and its process group was killed.
    TimedOut,
    /// The run was cancelled: it ended on a signal, or waiting on it failed.
    Cancelled,
    /// The process never started (a missing command, a sandbox construction error).
    FailedToStart,
}

impl HookRunEnd {
    /// The record's end text, from the one closed set.
    pub fn label(&self) -> String {
        match self {
            Self::Status(code) => format!("{HOOK_RUN_END_STATUS} {code}"),
            Self::TimedOut => HOOK_RUN_END_TIMED_OUT.to_owned(),
            Self::Cancelled => HOOK_RUN_END_CANCELLED.to_owned(),
            Self::FailedToStart => HOOK_RUN_END_FAILED_TO_START.to_owned(),
        }
    }
}

/// One finished hook run, in memory only. `line()` is the one record format the `/hooks`
/// page, the log and the tests read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunRecord {
    /// `PreToolUse`, `PostToolUse` or `Stop`.
    pub event: String,
    /// The tool name for a tool event; `None` for `Stop`.
    pub tool: Option<String>,
    /// The command's first line.
    pub command: String,
    pub end: HookRunEnd,
    /// The run's duration in milliseconds, rounded up so a fast run never reads `0`.
    pub millis: u64,
}

impl HookRunRecord {
    /// `<event>:<tool> [<command first line>] <end> (<n>ms)`; `Stop` has no tool name.
    pub fn line(&self) -> String {
        let event = match &self.tool {
            Some(tool) => format!("{}:{tool}", self.event),
            None => self.event.clone(),
        };
        format!(
            "{event} [{}] {} ({}ms)",
            self.command,
            self.end.label(),
            self.millis
        )
    }
}

/// One compiled `{ matcher, hooks }` entry: the regex is built once at load time, so an
/// invalid pattern warns once there and never runs (never per call).
#[derive(Clone, Debug)]
struct CompiledEntry {
    hooks: Vec<HookCommand>,
    matcher: CompiledMatcher,
}

/// Claude's case-sensitive matcher: `*`/empty/missing matches every tool, otherwise a
/// regular expression anchored to the whole tool name. `Never` is an invalid pattern:
/// warned once at load, never runs.
#[derive(Clone, Debug)]
enum CompiledMatcher {
    All,
    Regex(regex::Regex),
    Never,
}

/// One compiled observe-only entry (ADR 0045 §6): its matcher is a value of a closed set
/// (`*`/empty matches every value), never a regex.
#[derive(Clone, Debug)]
struct CompiledObserveEntry {
    hooks: Vec<HookCommand>,
    matcher: ObserveMatcher,
}

/// The `StopFailure`/`Notification` matcher after load: `All`, one closed-set value, or
/// `Never` for a value outside the set (warned once at load, never run).
#[derive(Clone, Debug)]
enum ObserveMatcher {
    All,
    Value(String),
    Never,
}

impl ObserveMatcher {
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::All => true,
            Self::Value(allowed) => allowed == value,
            Self::Never => false,
        }
    }
}

/// One async hook process running right now. It is not a job: the model cannot `wait` or
/// `kill` it, `/jobs` never lists it, and only the `/hooks` page's "Running" section and
/// `Stop`'s `background_tasks` show it (ADR 0045 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningHook {
    /// The registry id (`hook-…`), unique within the session.
    pub id: String,
    /// `PostToolUse` or `Stop`.
    pub event: String,
    /// The tool name for `PostToolUse`; `None` for `Stop`.
    pub tool: Option<String>,
    /// The command as written (the first line is shown).
    pub command: String,
    /// When the process started.
    pub started: Instant,
}

impl RunningHook {
    /// The one page line: the run-record shape with `running` in place of the end.
    pub fn line(&self) -> String {
        let event = match &self.tool {
            Some(tool) => format!("{}:{tool}", self.event),
            None => self.event.clone(),
        };
        let millis = u64::try_from(self.started.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        format!(
            "{event} [{}] running ({millis}ms)",
            first_line(&self.command)
        )
    }
}

/// The identity the supersede rule uses (ADR 0045 §5): the event plus the same `command` and
/// `args` pair the dedup key uses, so a project and a user entry naming one hook are one
/// hook. The tool name is not part of it: one `PostToolUse` hook matched for two different
/// tools is still the same hook.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AsyncHookKey {
    event: String,
    command: String,
    args: Option<Vec<String>>,
}

impl AsyncHookKey {
    fn of(hook: &HookCommand, event: &str) -> Self {
        Self {
            event: event.to_owned(),
            command: hook.command.clone(),
            args: hook.args.clone(),
        }
    }
}

/// The handshake between one async run's own task and the code that supersedes it or ends
/// the session.
#[derive(Debug, Default)]
struct AsyncRunHandle {
    /// The child's process-group leader pid, `0` until the process has started (or when it
    /// never started).
    pid: AtomicU32,
    /// A supersede or the session end cancelled this run: it delivers nothing.
    cancelled: AtomicBool,
    /// The process has exited (or never started); its result may still be delivering. A run
    /// past this point is never superseded, because only a still-running process is.
    exited: AtomicBool,
}

/// One async hook in the running registry: the `/hooks` page's view plus what supersede and
/// the session end need (ADR 0045 §5).
#[derive(Debug)]
struct AsyncRun {
    running: RunningHook,
    key: AsyncHookKey,
    handle: Arc<AsyncRunHandle>,
}

/// One observe-only run in flight: its process handle, and whether `exec -p` waits for it
/// (only `StopFailure`; a `Notification` hook never postpones an exit).
#[derive(Debug)]
struct ObserveRun {
    handle: Arc<AsyncRunHandle>,
    waits_on_exit: bool,
}

/// The user-level command hooks: one [`Hooks`] implementation for every event, plus the
/// `PermissionRequest` chain the permission layer calls through
/// [`radian_permissions::PermissionRequestHooks`].
#[derive(Clone)]
pub struct CommandHooks {
    pre: Vec<CompiledEntry>,
    /// The `PermissionRequest` chain (ADR 0045 §4): project entries first, then the user's.
    /// Implemented as [`radian_permissions::PermissionRequestHooks`] so the permission layer
    /// can call it without depending on this crate.
    permission_request: Vec<CompiledEntry>,
    post: Vec<CompiledEntry>,
    stop: Vec<HookMatcher>,
    /// The observe-only `StopFailure` entries (ADR 0045 §6), matched by `ModelErrorKind`.
    stop_failure: Vec<CompiledObserveEntry>,
    /// The observe-only `Notification` entries (ADR 0045 §6), matched by notification type.
    notification: Vec<CompiledObserveEntry>,
    workspace: PathBuf,
    session_id: String,
    sandbox: SandboxSlot,
    inbox: Arc<Inbox>,
    runtime: tokio::runtime::Handle,
    /// The sliding-window wake budget (ADR 0037 decision 5's rate gate): Stop
    /// continuations inside one window, expiring on their own with no user message
    /// needed. Each `CommandHooks` holds its own count (see the follow-up on
    /// `crate::job_notify::JobNotifier`): the same rate rule, not a shared counter.
    gate: Arc<WakeGate>,
    /// Turns started and not resolved yet: a spawned Stop run still going, or a turn
    /// whose `on_turn_end` has not run. `exec -p` waits for these before deciding the
    /// run is over — the count is raised synchronously at turn start, strictly before
    /// the turn can end, so there is no gap for the exit check to slip through.
    pending: Arc<AtomicU64>,
    /// Bumped whenever `pending` drops, so a waiter re-checks.
    pending_signal: Arc<Notify>,
    /// Continuations degraded to inject-only since the last wake. A woken turn absorbs
    /// the waiting injects, so a wake ends the burst; a user message resets it at once.
    degraded: Arc<AtomicU32>,
    /// The built-in intent-stop validator (backlog `turn-end-continuation.md` §B): on when
    /// `[hooks] intent_stop` is unset or true. It runs even with no `Stop` hook configured.
    intent_stop: bool,
    /// The background-job registry, for the "no background job running" rule. `Weak`
    /// (like the notifier's): a missing or dropped registry reads as no jobs running.
    /// The host sets it with [`CommandHooks::set_jobs`] when it has a registry.
    jobs: Arc<std::sync::Mutex<Option<Weak<Jobs>>>>,
    /// The persistent goal: an `Active` or `Paused` goal owns the turn end (it re-posts
    /// itself), so the validator stays off while one is set. The host sets it with
    /// [`CommandHooks::set_goal`].
    goal: Arc<std::sync::Mutex<Option<SharedGoal>>>,
    /// The TUI `/loop` mode owns the turn end while armed, so the validator stays off.
    /// The TUI loop syncs this after every update; `exec` never sets it (it has no loop).
    loop_active: Arc<AtomicBool>,
    /// The message ids of `Stop` continuations sent and not yet claimed, with the verdict
    /// that drove each: `true` when a hook did (exit 2, `decision: "block"`,
    /// `additionalContext`), `false` for the built-in validator alone. Provenance travels
    /// with the queued message: [`Inbox::claim_next_turn`]'s callback removes the id and
    /// marks the turn it opens, so a slow `Stop` run for another turn can never consume it.
    /// A user message clears the map ([`CommandHooks::reset_wakes`]): a continuation still
    /// queued when the user speaks is no longer a continuation.
    pending_continuations: Arc<std::sync::Mutex<HashMap<MessageId, bool>>>,
    /// What opened the turn now running, read once at that turn's own `on_turn_end`
    /// (`stop_hook_active` on its `Stop` stdin, and the idle-continuation count). A
    /// non-completed turn clears it; a user message does not touch a turn that already
    /// started.
    turn_continuation: Arc<AtomicU8>,
    /// The permission layer, for the turn-source reset and lowering the turn-claimed and
    /// drained observers do. The host installs it with
    /// [`CommandHooks::set_turn_source_hooks`]; absent (tests that build the hooks alone),
    /// the observers skip their work and a turn-starting input keeps the source the
    /// enqueuer set.
    turn_source_hooks: Arc<std::sync::Mutex<Option<Weak<PermissionHooks>>>>,
    /// Consecutive hook-opened continuations whose turns ended `Completed` with no tool
    /// call (`model_calls == 1`). A continued turn that called a tool, a non-`Completed`
    /// turn, and a user message all reset it. At [`MAX_HOOK_IDLE_CONTINUATIONS`] the next
    /// hook continuation degrades instead of opening a turn.
    hook_idle: Arc<AtomicU32>,
    /// Consecutive validator-fired turn ends. The third consecutive hit reports `blocked`
    /// instead of continuing. A rule miss, a non-completed turn, or a user message
    /// ([`CommandHooks::reset_wakes`]) clears it.
    consecutive: Arc<AtomicU32>,
    /// The last [`HOOK_RUN_HISTORY`] finished runs, oldest first (newest at the back); shared
    /// across clones, one ring per session. Never journaled.
    runs: Arc<std::sync::Mutex<VecDeque<HookRunRecord>>>,
    /// The async hooks running right now, in spawn order. Not the [`Jobs`] registry: these
    /// are never model-visible jobs (ADR 0045 §5).
    async_running: Arc<std::sync::Mutex<Vec<AsyncRun>>>,
    /// The observe-only runs (`StopFailure`, `Notification`) in flight (ADR 0045 §6). They
    /// are not async hooks: they never enter the jobs registry, `Stop`'s `background_tasks`,
    /// or supersede one another. `exec -p` waits only for the `StopFailure` runs (at most
    /// 5 s); a `Notification` run never delays an exit.
    observe_running: Arc<std::sync::Mutex<Vec<ObserveRun>>>,
    /// Bumped whenever one observe run finishes or is killed, so a waiter re-checks.
    observe_signal: Arc<Notify>,
    /// Bumped whenever one async hook finishes (or fails to start), so a waiter re-checks.
    async_signal: Arc<Notify>,
    /// The id sequence for [`RunningHook::id`].
    async_seq: Arc<AtomicU64>,
    /// The session ended (its driver is joined): every running async hook was killed, its
    /// result is never delivered, and no new one starts (ADR 0045 §5).
    async_ended: Arc<AtomicBool>,
    /// Serializes async-result delivery against the session end (ADR 0045 §5). A delivery
    /// holds it from the `async_ended` check through the inbox enqueue, and
    /// [`CommandHooks::end_session`] holds it while it sets the flag and drains the registry:
    /// a result cannot pass the check and then enqueue after the end began, and the end does
    /// not return while a delivery is still enqueuing, so nothing reaches the old session
    /// afterwards.
    deliver_gate: Arc<tokio::sync::Mutex<()>>,
    warn: HooksWarning,
    /// The wake-budget notice line (routed like `warn`, as its own transcript notice).
    wake: HooksWarning,
    /// The two-in-a-row cap's notice line (its own transcript notice, so the TUI can mark
    /// the session `intent stop` for herdr until the next turn starts).
    intent_stop_warn: HooksWarning,
    /// The idle-continuation cap's notice line (its own transcript notice, so the TUI can
    /// mark the session `hook stop` for herdr until the next turn starts).
    hook_stop_warn: HooksWarning,
}
/// A process-wide counter for continuation message ids. The wall clock alone is not
/// enough (two calls can share a nanosecond, as in the inbox's own id nonce), so the
/// sequence keeps ids unique within a process; the pid keeps them unique across runs.
static CONTINUATION_SEQ: AtomicU64 = AtomicU64::new(0);

/// The id of a `Stop` continuation message. The host must know it before the inbox
/// accepts the message: the id is what ties the queued followup to the turn it opens
/// (`stop_hook_active`, ADR 0045 §2). A caller-supplied id is kept by the inbox.
fn continuation_message_id() -> MessageId {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let seq = CONTINUATION_SEQ.fetch_add(1, Ordering::Relaxed);
    MessageId(format!("stop-{:x}{:x}{:x}", std::process::id(), nanos, seq))
}

/// Installs the inbox's turn-claimed observer: the id of each message that opens a turn
/// is looked up in the pending continuation map, and the verdict that drove it becomes
/// the running turn's provenance. A claim of any other input clears it (a new turn). It is
/// also the turn-start seam for the permission source (ADR 0011 L2, ADR 0043): the turn
/// runs with the least-privileged source of the input that opened it and of every waking
/// `steer` already queued for it, so a `User` input queued mid-turn
/// cannot raise the rights of a turn that also absorbs a `Scheduler`/`Mcp`/`Subagent`
/// input, and never resets while another turn runs. A plain `inject` waits for a natural
/// step boundary; the drain callback below lowers the turn when that safe point admits it.
fn install_turn_claimed_callback(
    inbox: &Arc<Inbox>,
    pending: Arc<std::sync::Mutex<HashMap<MessageId, bool>>>,
    turn_continuation: Arc<AtomicU8>,
    turn_source_hooks: Arc<std::sync::Mutex<Option<Weak<PermissionHooks>>>>,
) {
    let source_hooks = Arc::clone(&turn_source_hooks);
    inbox.set_turn_claimed_callback(Some(Arc::new(
        move |entry: &InboxEntry, queued: &[InputSource]| {
            // The source reset comes first, before any turn work can read the source. The
            // opener sets the turn's starting point; every queued waking entry the turn will
            // admit can only lower it. A plain `inject` is not waking, so it is not in
            // `queued`: the turn lowers for it when the safe point admits it (the drain
            // callback below), before the next model request.
            let hooks = source_hooks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref()
                .and_then(Weak::upgrade);
            if let Some(hooks) = hooks {
                hooks.set_turn_source(Some(entry.source));
                for source in queued {
                    hooks.observe_input_source(*source);
                }
            }
            let continuation = entry.message.meta().id.as_ref().and_then(|id| {
                pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(id)
            });
            let kind = match continuation {
                Some(true) => Continuation::Hook,
                Some(false) => Continuation::Validator,
                None => Continuation::None,
            };
            turn_continuation.store(kind.as_u8(), Ordering::SeqCst);
        },
    )));
    // A `steer`/`inject` admitted at a safe point joins the turn: its source applies then,
    // so an owner turn that absorbs a non-human input loses the human rights before the
    // next model call (ADR 0011 L2).
    inbox.set_input_drained_callback(Some(Arc::new(move |source: InputSource| {
        if let Some(hooks) = turn_source_hooks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
        {
            hooks.observe_input_source(source);
        }
    })));
}

/// Writes `id` into `message`'s host metadata (the inbox keeps a caller-supplied id).
fn set_message_id(message: &mut Message, id: MessageId) {
    match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.id = Some(id),
    }
}

impl CommandHooks {
    /// Builds the hooks from the user `[hooks]` section layered over the project hooks
    /// (`<workspace>/.radian/hooks.json`, empty when untrusted). Each event compiles the
    /// project entries first, then the user's, so at run time the project layer goes first
    /// and the user's hooks see the final arguments. Empty when both layers configured
    /// nothing, in which case every method is a no-op.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &HooksConfig,
        project: &HooksConfig,
        workspace: &Path,
        session_id: &str,
        sandbox: SandboxSlot,
        inbox: &Arc<Inbox>,
        runtime: &tokio::runtime::Handle,
        warn: HooksWarning,
        wake: HooksWarning,
        intent_stop_warn: HooksWarning,
        hook_stop_warn: HooksWarning,
    ) -> Self {
        // The continuation provenance rides on the claimed message, not on a session-global
        // pending flag: the inbox reports each turn-opening claim, and only the turn that
        // claims a continuation message reports `stop_hook_active: true` (ADR 0045 §2).
        let pending_continuations: Arc<std::sync::Mutex<HashMap<MessageId, bool>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let turn_continuation = Arc::new(AtomicU8::new(Continuation::None.as_u8()));
        let turn_source_hooks: Arc<std::sync::Mutex<Option<Weak<PermissionHooks>>>> =
            Arc::new(std::sync::Mutex::new(None));
        install_turn_claimed_callback(
            inbox,
            Arc::clone(&pending_continuations),
            Arc::clone(&turn_continuation),
            Arc::clone(&turn_source_hooks),
        );
        Self {
            pre: compile_layered(
                &project.pre_tool_use,
                &config.pre_tool_use,
                "PreToolUse",
                &warn,
            ),
            permission_request: compile_layered(
                &project.permission_request,
                &config.permission_request,
                "PermissionRequest",
                &warn,
            ),
            post: compile_layered(
                &project.post_tool_use,
                &config.post_tool_use,
                "PostToolUse",
                &warn,
            ),
            stop: project
                .stop
                .iter()
                .chain(config.stop.iter())
                .cloned()
                .collect(),
            stop_failure: compile_observe_layered(
                &project.stop_failure,
                &config.stop_failure,
                "StopFailure",
                STOP_FAILURE_MATCHERS,
                &warn,
            ),
            notification: compile_observe_layered(
                &project.notification,
                &config.notification,
                "Notification",
                NOTIFICATION_TYPES,
                &warn,
            ),
            workspace: workspace.to_path_buf(),
            session_id: session_id.to_owned(),
            sandbox,
            inbox: Arc::clone(inbox),
            runtime: runtime.clone(),
            gate: Arc::new(WakeGate::new()),
            degraded: Arc::new(AtomicU32::new(0)),
            pending: Arc::new(AtomicU64::new(0)),
            pending_signal: Arc::new(Notify::new()),
            intent_stop: config
                .intent_stop
                .unwrap_or(crate::config::DEFAULT_INTENT_STOP),
            jobs: Arc::new(std::sync::Mutex::new(None)),
            goal: Arc::new(std::sync::Mutex::new(None)),
            loop_active: Arc::new(AtomicBool::new(false)),
            pending_continuations,
            turn_continuation,
            turn_source_hooks,
            hook_idle: Arc::new(AtomicU32::new(0)),
            consecutive: Arc::new(AtomicU32::new(0)),
            runs: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            async_running: Arc::new(std::sync::Mutex::new(Vec::new())),
            observe_running: Arc::new(std::sync::Mutex::new(Vec::new())),
            observe_signal: Arc::new(Notify::new()),
            async_signal: Arc::new(Notify::new()),
            async_seq: Arc::new(AtomicU64::new(0)),
            async_ended: Arc::new(AtomicBool::new(false)),
            deliver_gate: Arc::new(tokio::sync::Mutex::new(())),
            warn,
            wake,
            intent_stop_warn,
            hook_stop_warn,
        }
    }

    /// Whether no hook at all is configured in either layer.
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty()
            && self.permission_request.is_empty()
            && self.post.is_empty()
            && self.stop.is_empty()
            && self.stop_failure.is_empty()
            && self.notification.is_empty()
    }

    /// A user message arrived: the `Stop` continuation budget starts over (the same reset
    /// the background-job notifier gets, ADR 0037 decision 3). A degraded continuation
    /// still runs the hook (its exit-2 verdict degrades); only the *wake* is gated. The
    /// validator's consecutive-hit count starts over too, and a continuation still queued
    /// is no longer one: its message id leaves the pending set, so if a turn claims it
    /// later that turn reports `stop_hook_active: false` (ADR 0045 §2). A turn that
    /// already started keeps the provenance it was claimed with.
    pub fn reset_wakes(&self) {
        self.gate.reset_wakes();
        self.degraded.store(0, Ordering::SeqCst);
        self.consecutive.store(0, Ordering::SeqCst);
        self.hook_idle.store(0, Ordering::SeqCst);
        self.pending_continuations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    /// The background-job registry for the "no background job running" rule. The host
    /// calls this when it has a registry; a missing or dropped registry reads as no
    /// jobs running.
    pub fn set_jobs(&self, jobs: Option<&Arc<Jobs>>) {
        *self
            .jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = jobs.map(Arc::downgrade);
    }

    /// The permission layer whose turn source the claimed-input observer resets. The host
    /// calls this once, right after it builds the hooks; a `Weak` keeps the session from
    /// holding itself alive through the inbox's callback.
    pub fn set_turn_source_hooks(&self, hooks: Weak<PermissionHooks>) {
        *self
            .turn_source_hooks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(hooks);
    }

    /// The persistent goal for the active-goal suppression (an `Active` or `Paused` goal
    /// owns the turn end). The host calls this with its shared goal.
    pub fn set_goal(&self, goal: SharedGoal) {
        *self
            .goal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(goal);
    }

    /// Whether the TUI `/loop` mode is armed (it owns the turn end while it is). The TUI
    /// loop syncs this after every update; `exec` never sets it.
    pub fn set_loop_active(&self, active: bool) {
        self.loop_active.store(active, Ordering::SeqCst);
    }

    /// Whether the TUI `/loop` mode is armed (tests).
    pub fn loop_active(&self) -> bool {
        self.loop_active.load(Ordering::SeqCst)
    }

    /// Consecutive validator-fired turn ends (tests): the third consecutive hit reports
    /// `blocked` instead of continuing.
    pub fn consecutive_hits(&self) -> u32 {
        self.consecutive.load(Ordering::SeqCst)
    }

    /// Consecutive hook-opened continuations whose turns did no work (tests): at
    /// [`MAX_HOOK_IDLE_CONTINUATIONS`] the next hook continuation degrades.
    pub fn hook_idle_continuations(&self) -> u32 {
        self.hook_idle.load(Ordering::SeqCst)
    }

    /// Whether the turn now running was opened by a `Stop` continuation (tests); its
    /// `Stop` stdin reports this as `stop_hook_active`.
    pub fn stop_hook_active(&self) -> bool {
        Continuation::from_u8(self.turn_continuation.load(Ordering::SeqCst)).is_continuation()
    }

    /// Background jobs running right now (tests). A missing or dropped registry reads
    /// as none.
    pub fn jobs_running(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|jobs| jobs.settle_snapshot().0)
            .unwrap_or(0)
    }

    /// `Stop` continuations inside the window right now (tests).
    pub fn stop_wakes(&self) -> u32 {
        self.gate.wakes()
    }

    /// The last [`HOOK_RUN_HISTORY`] hook runs, newest first (the `/hooks` page's Recent
    /// runs section). In memory only: `radian exec` has no page and reads only the log line.
    pub fn hook_runs(&self) -> Vec<HookRunRecord> {
        self.runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    /// The async hooks running right now, in spawn order (the `/hooks` page's Running
    /// section and `Stop`'s `background_tasks`). In memory only.
    pub fn running_async_hooks(&self) -> Vec<RunningHook> {
        self.async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|run| run.running.clone())
            .collect()
    }

    /// How many async hooks are running right now. The intent-stop validator counts these
    /// like background jobs: no trigger while one is in flight (ADR 0045 §5).
    pub fn async_hooks_running(&self) -> usize {
        self.async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    /// Resolves whenever one async hook finishes. A waiter re-checks its exit condition on
    /// each one (tests and a host that wants to drain them).
    pub fn async_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.async_signal)
    }

    /// Fires the `StopFailure` observers for one turn that ended on a model error
    /// (ADR 0045 §6). Called by the host's runtime-event layer from the driver's
    /// `on_model_error` path, once per failed turn. The entries whose matcher names this
    /// kind (or `*`) start in the background and return at once; the turn, its exit code and
    /// its output never wait for them.
    pub fn run_stop_failure(&self, error: &ModelError, last_assistant_message: &str) {
        if self.stop_failure.is_empty() {
            return;
        }
        let kind = error.kind().as_str();
        let stdin = serde_json::json!({
            "hook_event_name": "StopFailure",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
            "error": kind,
            "error_details": short(&error.to_string()),
            "last_assistant_message": last_assistant_message,
        });
        self.observe_entries("StopFailure", &self.stop_failure, kind, &stdin);
    }

    /// Fires the TUI's `idle_prompt` observers (ADR 0045 §6): a turn ended and no input
    /// arrived for the idle period. `exec` never calls this (it exits at the turn end).
    pub fn notify_idle(&self) {
        NotificationHooks::notification(self, NOTIFICATION_IDLE_PROMPT, None);
    }

    /// How many continuations were degraded since the last wake (tests). A woken turn
    /// absorbs the waiting injects, so a wake resets this to zero; a user message does
    /// too.
    pub fn degraded(&self) -> u32 {
        self.degraded.load(Ordering::SeqCst)
    }

    /// How many continuations are degraded and still waiting: a verdict no turn will
    /// ever start for. `exec -p` ends the session on these (exit 3) instead of waiting.
    pub fn degraded_pending(&self) -> u64 {
        u64::from(self.degraded.load(Ordering::SeqCst))
    }

    /// A turn started: [`Hooks::on_turn_end`] will balance this (it keeps the count
    /// when it spawns a Stop run, drops it otherwise). Called synchronously from the
    /// turn-start event, strictly before the turn can end — so a host that sees the
    /// turn end also sees a pending count while the verdict is unresolved. (There is
    /// no such guarantee for work `on_turn_end` itself starts: `RunFinished` is
    /// emitted before it runs.)
    pub fn note_turn_started(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    /// Stop turns started and not resolved yet: a spawned run still going, or a turn
    /// whose `on_turn_end` has not run. (Tests drive `run_stop_once` directly, so only
    /// `on_turn_end` and `note_turn_started` move this.)
    pub fn pending_stops(&self) -> u64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// Resolves whenever `pending_stops` drops (a spawned Stop run finished, or a turn
    /// ended with no Stop run coming). A waiter re-checks its exit condition on each
    /// one.
    pub fn stops_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.pending_signal)
    }

    /// Whether a compiled matcher matches a Radian tool name.
    fn matches(matcher: &CompiledMatcher, tool: &str) -> bool {
        match matcher {
            CompiledMatcher::All => true,
            CompiledMatcher::Never => false,
            CompiledMatcher::Regex(regex) => regex.is_match(tool),
        }
    }
}

/// The dedup key of one hook (ADR 0045 §1 item 3): the command plus its whole `args` list,
/// so the same program with different arguments is two hooks, and two layers writing the
/// same command and arguments (or both without arguments) run once.
fn dedup_key(hook: &HookCommand) -> (String, Option<Vec<String>>) {
    (hook.command.clone(), hook.args.clone())
}

/// The command's first line, for a run record.
fn first_line(command: &str) -> String {
    command.lines().next().unwrap_or_default().to_owned()
}

/// The marker a bounded text ends with when it is cut. It is part of the bound, so a
/// truncated result is at most `REASON_CHAR_LIMIT` characters in total.
const TRUNCATION_MARKER: &str = "…[truncated]";

/// Bounds hook stderr before it enters the transcript.
fn short(text: &str) -> String {
    if text.chars().count() <= REASON_CHAR_LIMIT {
        return text.to_owned();
    }
    let keep = REASON_CHAR_LIMIT - TRUNCATION_MARKER.chars().count();
    let kept: String = text.chars().take(keep).collect();
    format!("{kept}{TRUNCATION_MARKER}")
}
