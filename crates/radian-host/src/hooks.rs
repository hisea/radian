//! User-level external command hooks (ADR 0045 slice A): the Claude/Codex subset as an
//! [`radian_core::Hooks`] implementation.
//!
//! Four events, all shell commands from the user's own `config.toml` (`[hooks]`):
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
//!
//! Async hooks are not jobs: they never enter the [`Jobs`] registry, `/jobs` or the `job`
//! tool. The `/hooks` page shows them under "Running", `Stop` lists each one in
//! `background_tasks` with `type: "hook"`, and the intent-stop validator treats one like a
//! running job (no trigger while any is in flight).
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

use crate::config::{HookCommand, HookMatcher, HooksConfig};
use crate::goal::SharedGoal;
use crate::intent_stop;
use crate::wake_gate::WakeGate;
use radian_coding::Jobs;
use radian_core::{
    ContentPart, HookContext, HookError, Hooks, InputSource, Message, MessageId, RunOutcome,
    RunStatus, ToolCall, ToolDecision, ToolResult,
};
use radian_permissions::{PermissionRequestDecision, PermissionRequestHooks};
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
    async_running: Arc<std::sync::Mutex<Vec<RunningHook>>>,
    /// Bumped whenever one async hook finishes (or fails to start), so a waiter re-checks.
    async_signal: Arc<Notify>,
    /// The id sequence for [`RunningHook::id`].
    async_seq: Arc<AtomicU64>,
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
/// the running turn's provenance. A claim of any other input clears it (a new turn).
fn install_turn_claimed_callback(
    inbox: &Arc<Inbox>,
    pending: Arc<std::sync::Mutex<HashMap<MessageId, bool>>>,
    turn_continuation: Arc<AtomicU8>,
) {
    inbox.set_turn_claimed_callback(Some(Arc::new(move |entry: &InboxEntry| {
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
        install_turn_claimed_callback(
            inbox,
            Arc::clone(&pending_continuations),
            Arc::clone(&turn_continuation),
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
            hook_idle: Arc::new(AtomicU32::new(0)),
            consecutive: Arc::new(AtomicU32::new(0)),
            runs: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            async_running: Arc::new(std::sync::Mutex::new(Vec::new())),
            async_signal: Arc::new(Notify::new()),
            async_seq: Arc::new(AtomicU64::new(0)),
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
            .clone()
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

    /// Background work that makes the intent-stop validator stand down: the job registry
    /// plus every running async hook (ADR 0045 §3/§5).
    fn background_running(&self) -> usize {
        self.jobs_running() + self.async_hooks_running()
    }

    /// Records one finished run: one `tracing` info line, then the in-memory ring (the
    /// oldest entry drops when it is full). Milliseconds are rounded up, so a fast hook
    /// never reads `0ms`.
    fn record_run(
        &self,
        event: &str,
        tool: Option<&str>,
        command: &str,
        end: HookRunEnd,
        started: Instant,
    ) {
        let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let record = HookRunRecord {
            event: event.to_owned(),
            tool: tool.map(str::to_owned),
            command: first_line(command),
            end,
            millis: micros.div_ceil(1000).max(1),
        };
        tracing::info!(target: "radian_host::hooks", "{}", record.line());
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while runs.len() >= HOOK_RUN_HISTORY {
            runs.pop_front();
        }
        runs.push_back(record);
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

    /// Runs one `Stop` cycle synchronously: every configured Stop hook in order — the
    /// project layer first, then the user's (ADR 0045 slice B), each command string once —
    /// plus the built-in intent-stop validator (backlog `turn-end-continuation.md` §B),
    /// which runs even with no `Stop` hook configured. Every exit 2 collects its stderr;
    /// after the loop a single continuation opens with the reasons joined in one message,
    /// counted as one wake. Past the continuation cap the verdict degrades to a marked
    /// `inject` (no wake) with the wake-budget notice, so it waits for the next
    /// user-woken turn instead of being dropped silently. Only [`RunStatus::Completed`]
    /// runs the hooks: a cancelled, failed, or otherwise non-completed turn runs nothing
    /// (an exit-2 hook after a user interrupt would otherwise open a turn the user just
    /// asked to end).
    /// [`Hooks::on_turn_end`] runs this cycle with the provenance it captured at that
    /// turn's boundary (`run_stop_once_with`); tests drive this entry directly so there
    /// is no timing involved.
    pub async fn run_stop_once(&self, outcome: &RunOutcome) {
        // The value says what opened *this* turn (nothing, the built-in validator, or a
        // hook); it is consumed once, by this turn's own `Stop` (ADR 0045 §2).
        let continuation = self.take_turn_continuation();
        self.run_stop_once_with(outcome, continuation).await;
    }

    /// Reads and clears the running turn's continuation provenance.
    fn take_turn_continuation(&self) -> Continuation {
        Continuation::from_u8(
            self.turn_continuation
                .swap(Continuation::None.as_u8(), Ordering::SeqCst),
        )
    }

    /// Counts the turn that just ended: a hook-opened continuation with no tool call adds
    /// one to the idle streak; a continued turn that called a tool resets it; any other
    /// turn (not continued, or opened by the validator alone) ends the hook streak too.
    fn count_idle_continuation(&self, outcome: &RunOutcome, continuation: Continuation) {
        if continuation == Continuation::Hook && outcome.model_calls == 1 {
            self.hook_idle.fetch_add(1, Ordering::SeqCst);
        } else {
            self.hook_idle.store(0, Ordering::SeqCst);
        }
    }

    /// [`CommandHooks::run_stop_once`] with the provenance already captured: the turn-end
    /// hook reads it synchronously, before spawning, so a slow `Stop` run for an earlier
    /// turn can never consume this turn's value.
    async fn run_stop_once_with(&self, outcome: &RunOutcome, continuation: Continuation) {
        // A non-completed turn runs no `Stop` and opens no turn of its own.
        if !matches!(outcome.status, RunStatus::Completed) {
            return;
        }
        // The idle-continuation streak follows this turn's own outcome: a hook continuation
        // whose turn did no work adds one; real work (a tool call) resets it.
        self.count_idle_continuation(outcome, continuation);
        // Claude's field name: the last assistant message's text parts joined, so a user
        // script can run its own detection (backlog §B.6). Empty when the turn has none.
        let last_message = outcome
            .final_message
            .as_ref()
            .map(Message::text)
            .unwrap_or_default();
        let stdin = serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
            "last_assistant_message": last_message,
            "stop_hook_active": continuation.is_continuation(),
            "background_tasks": self.background_tasks(),
            "session_crons": self.session_crons(),
        });
        let mut reasons = Vec::new();
        // One run per command string, like the tool-call events (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.stop {
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                // `background_tasks` is read per hook, so a sync hook later in the chain
                // sees an async hook that started just before it (ADR 0045 §5).
                let mut hook_stdin = stdin.clone();
                hook_stdin["background_tasks"] = serde_json::Value::Array(self.background_tasks());
                // An async `Stop` hook is not part of this turn's continuation chain and
                // is not counted by the idle-continuation counter (ADR 0045 §5): it starts
                // and the chain continues at once, delivering its own result later.
                if hook.is_async() {
                    self.spawn_async(hook, hook_stdin, "Stop", None);
                    continue;
                }
                match self.execute(hook, &hook_stdin, "Stop", None).await {
                    Ok(output) if output.code == Some(0) => {
                        if let Some(parsed) = self.parsed_output(&output.stdout, "Stop") {
                            reasons.extend(self.stop_output_reasons(parsed));
                        }
                    }
                    Ok(output) if output.code == Some(2) => {
                        let reason = short(&output.stderr);
                        reasons.push(if reason.trim().is_empty() {
                            "the Stop hook exited with code 2".to_owned()
                        } else {
                            reason
                        });
                    }
                    Ok(output) => {
                        // Fail closed (ADR 0017): a broken validator never continues the
                        // turn. The warning is the user-visible trace; the turn simply ends.
                        (self.warn)(&format!(
                            "Stop hook exited with code {}: {}",
                            output
                                .code
                                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                            short(&output.stderr),
                        ));
                        return;
                    }
                    Err(error) => {
                        (self.warn)(&format!("Stop hook failed: {error}"));
                        return;
                    }
                }
            }
        }
        // The built-in validator joins the same single continuation (at most one per turn):
        // its fixed text rides after the hook reasons, and its presence marks the message
        // with the `intent-stop` origin.
        let hook_drove = !reasons.is_empty();
        let validator_drove = self.validator_verdict(outcome).await;
        if validator_drove {
            reasons.push(intent_stop::INTENT_STOP_TEXT.to_owned());
        }
        if !hook_drove && !validator_drove {
            return;
        }
        let text = if hook_drove {
            format!("Stop hook requested another turn:\n{}", reasons.join("\n"))
        } else {
            reasons.join("\n")
        };
        let mut message = Message::user(text);
        if validator_drove {
            match &mut message {
                Message::User { meta, .. }
                | Message::Assistant { meta, .. }
                | Message::Tool { meta, .. } => {
                    meta.origin = Some(intent_stop::intent_stop_origin());
                }
            }
        }
        // THE IDLE-CONTINUATION CAP (ADR 0045 §3): the
        // continuation that would be the 8th hook continuation in a row of no-work turns
        // does not open one. It takes the validator's two-in-a-row treatment: a marked
        // `inject` (journal-visible, no wake), one notice line through the host's
        // `hook stop` channel, and the TUI's `blocked` label. A merged validator
        // continuation rides the same single message, so it degrades with it.
        let idle_capped =
            hook_drove && self.hook_idle.load(Ordering::SeqCst) + 1 >= MAX_HOOK_IDLE_CONTINUATIONS;
        if idle_capped {
            if self
                .inbox
                .inject_degraded(message, InputSource::System)
                .await
                .is_ok()
            {
                self.degraded.fetch_add(1, Ordering::SeqCst);
            }
            (self.hook_stop_warn)(&hook_stop_cap_line());
            return;
        }
        // THE WAKE-GATE DEGRADE POINT (ADR 0037 decision 5): past the gate
        // the hook still runs, but an exit-2 verdict degrades to a marked
        // `inject` (no wake) with the wake-budget notice, the same degrade
        // as the background-job notifier (ADR 0045 §2) — a skipped run
        // would be silent, a degraded verdict waits visibly.
        // One continuation for every exit-2 verdict in the cycle, with the
        // reasons joined above: one wake, one degrade count at most. The
        // validator rides this same point: its continuation counts one wake.
        if self.gate.wake_allowed() {
            // The woken turn absorbs the waiting injects, so a wake also
            // ends the degrade burst. The continuation carries its own message id, and
            // that id is what the claim consumes (ADR 0045 §2): the turn it opens is the
            // only one that reports `stop_hook_active: true`, however late this run
            // finishes. A failed enqueue opened nothing and drops the id.
            let continuation = continuation_message_id();
            set_message_id(&mut message, continuation.clone());
            self.pending_continuations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(continuation.clone(), hook_drove);
            if self
                .inbox
                .followup(message, InputSource::System)
                .await
                .is_ok()
            {
                self.gate.note_wake();
                self.degraded.store(0, Ordering::SeqCst);
            } else {
                self.pending_continuations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&continuation);
            }
        } else if self
            .inbox
            .inject_degraded(message, InputSource::System)
            .await
            .is_ok()
        {
            let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
            (self.wake)(&crate::job_notify::wake_budget_line(degraded));
        }
    }

    /// The built-in validator's verdict for this turn end: `true` when its fixed text
    /// joins the continuation below. A hit with fewer than [`MAX_INTENT_STOP_CONSECUTIVE`]
    /// consecutive predecessors counts the hit and returns `true`. Anything else — the
    /// switch off, `/loop` or a goal owning the turn end, a background job running, a
    /// rule miss — clears the count and returns `false`.
    ///
    /// The third consecutive hit takes THE TWO-IN-A-ROW CAP instead: no third
    /// continuation opens. The verdict degrades to a marked `inject` (journal-visible,
    /// no wake) and the host reports the stop — the TUI shows one notice line, herdr
    /// reports `blocked` with the `intent stop` label until the next turn starts, and a
    /// user message clears it all. The count stays saturated, so further consecutive
    /// hits re-report (rewriting the same TUI line) instead of opening turns.
    async fn validator_verdict(&self, outcome: &RunOutcome) -> bool {
        let hit = self.intent_stop
            && !self.loop_active.load(Ordering::SeqCst)
            && !self.goal_active()
            && self.background_running() == 0
            && outcome
                .final_message
                .as_ref()
                .is_some_and(intent_stop::matches_rule);
        if !hit {
            self.consecutive.store(0, Ordering::SeqCst);
            return false;
        }
        if self.consecutive.load(Ordering::SeqCst) >= MAX_INTENT_STOP_CONSECUTIVE {
            let message = intent_stop::intent_stop_message();
            if self
                .inbox
                .inject_degraded(message, InputSource::System)
                .await
                .is_ok()
            {
                self.degraded.fetch_add(1, Ordering::SeqCst);
            }
            (self.intent_stop_warn)(&intent_stop::intent_stop_cap_line());
            return false;
        }
        self.consecutive.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// The goal's status (`None` when no goal is set or the share was dropped).
    fn goal_status(&self) -> Option<radian_session::GoalStatus> {
        self.goal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .and_then(|goal| {
                goal.lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .as_ref()
                    .map(|state| state.status)
            })
    }

    /// Whether an `Active` or `Paused` goal owns the turn end (it re-posts itself, so
    /// the validator stays off). A missing goal reads as none.
    fn goal_active(&self) -> bool {
        matches!(
            self.goal_status(),
            Some(radian_session::GoalStatus::Active | radian_session::GoalStatus::Paused)
        )
    }

    /// The background jobs and async hooks running when `Stop` runs, as its
    /// `background_tasks` (ADR 0045 §2/§5). A registry that was dropped, or never set, reads
    /// as no jobs running: the hook still runs and the field is `[]`. An async hook is
    /// listed with `type: "hook"`, a job with `type: "shell"`.
    fn background_tasks(&self) -> Vec<serde_json::Value> {
        let jobs = {
            let slot = self
                .jobs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            slot.as_ref().and_then(Weak::upgrade)
        };
        let mut tasks: Vec<serde_json::Value> = Vec::new();
        if let Some(jobs) = jobs {
            tasks.extend(jobs.running_tasks().into_iter().map(|task| {
                serde_json::json!({
                    "id": task.id,
                    "type": "shell",
                    "status": "running",
                    "description": task.description,
                    "command": task.command,
                })
            }));
        }
        tasks.extend(self.running_async_hooks().into_iter().map(|hook| {
            serde_json::json!({
                "id": hook.id,
                "type": "hook",
                "status": "running",
                "description": first_line(&hook.command),
                "command": hook.command,
            })
        }));
        tasks
    }

    /// The session-level schedulers that open turns by themselves, as the `Stop` stdin
    /// `session_crons` (ADR 0045 §2): an armed `/loop`, then an `Active` or `Paused` goal.
    fn session_crons(&self) -> Vec<serde_json::Value> {
        let mut crons = Vec::new();
        if self.loop_active.load(Ordering::SeqCst) {
            crons.push(serde_json::json!({"id": "loop", "type": "loop"}));
        }
        match self.goal_status() {
            Some(radian_session::GoalStatus::Active) => crons.push(serde_json::json!({
                "id": "goal",
                "type": "goal",
                "status": "active",
            })),
            Some(radian_session::GoalStatus::Paused) => crons.push(serde_json::json!({
                "id": "goal",
                "type": "goal",
                "status": "paused",
            })),
            _ => {}
        }
        crons
    }

    /// Runs one hook command with `stdin_json` on its stdin, inside the session sandbox
    /// when the slot holds one. A spawn failure or a timeout is a hook error (fail closed);
    /// exit codes are the caller's to interpret. Every run records one [`HookRunRecord`]
    /// (ADR 0045 §4): `tool` is the Radian tool name for the tool events, `None` for
    /// `Stop`.
    ///
    /// The child is a plain `std` process (`SandboxCommand` cannot become a Tokio command):
    /// stdin is written and the output waited on a blocking thread, and a timeout kills the
    /// whole session group (`setsid` in `spawn_piped`) before reaping it.
    async fn execute(
        &self,
        hook: &HookCommand,
        stdin_json: &serde_json::Value,
        event: &str,
        tool: Option<&str>,
    ) -> Result<ProcOutput, HookError> {
        let started = Instant::now();
        let input = serde_json::to_string(stdin_json)
            .map_err(|error| HookError::new(format!("could not encode the hook input: {error}")))?;
        let mut std_child = match self.spawn_piped(hook) {
            Ok(child) => child,
            Err(error) => {
                self.record_run(
                    event,
                    tool,
                    &hook.command,
                    HookRunEnd::FailedToStart,
                    started,
                );
                return Err(HookError::new(format!(
                    "could not start the {event} hook: {error}"
                )));
            }
        };
        let pid = std_child.id();
        // Dropping this future — a cancelled turn, a Core race — must kill the whole
        // process group, not leave the hook running after nobody waits for its answer
        // (ADR 0045 §4; edge case 9 of `hooks-permission-request`).
        let mut group = KillGroupOnDrop::new(pid);
        let stdin = std_child.stdin.take();
        // `select!` (not `tokio::time::timeout`) keeps the join handle, so a timeout can
        // still reap the child after killing it instead of detaching the waiter.
        let mut waiter = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Some(mut stdin) = stdin {
                // A hook that never reads its stdin (e.g. `exit 2`) may already have
                // exited, closing the pipe: the write then fails with a broken pipe.
                // That is not a hook failure — the hook's own exit code still reports
                // its verdict — so a stdin write error is ignored, never fail-closed.
                // (Dropping `stdin` closes the pipe, so a reading hook sees EOF.)
                let _ = stdin.write_all(input.as_bytes());
            }
            std_child.wait_with_output()
        });
        let timeout_secs = hook.timeout().as_secs();
        let finished = tokio::select! {
            result = &mut waiter => Some(result),
            () = tokio::time::sleep(hook.timeout()) => None,
        };
        match finished {
            Some(Ok(Ok(output))) => {
                // The child is reaped; never touch its (possibly reused) group again.
                group.disarm();
                // A process killed by a signal has no exit code; that run was cancelled
                // (the timeout path below is the timed-out end).
                let end = output
                    .status
                    .code()
                    .map_or(HookRunEnd::Cancelled, HookRunEnd::Status);
                self.record_run(event, tool, &hook.command, end, started);
                Ok(ProcOutput {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                })
            }
            Some(Ok(Err(error))) => {
                self.record_run(event, tool, &hook.command, HookRunEnd::Cancelled, started);
                Err(HookError::new(format!(
                    "the {event} hook could not finish: {error}"
                )))
            }
            Some(Err(join)) => {
                self.record_run(event, tool, &hook.command, HookRunEnd::Cancelled, started);
                Err(HookError::new(format!(
                    "the {event} hook task failed: {join}"
                )))
            }
            None => {
                kill_tree(pid);
                // The kill ends the child, so the waiter returns and reaps it; the
                // result no longer matters.
                let _ = waiter.await;
                group.disarm();
                self.record_run(event, tool, &hook.command, HookRunEnd::TimedOut, started);
                Err(HookError::new(format!(
                    "the {event} hook timed out after {timeout_secs}s"
                )))
            }
        }
    }

    /// Starts one async hook and returns at once: the process is registered as running and
    /// the run happens on the session runtime (ADR 0045 §5). The tool result or the `Stop`
    /// chain never waits for it.
    fn spawn_async(
        &self,
        hook: &HookCommand,
        stdin_json: serde_json::Value,
        event: &str,
        tool: Option<&str>,
    ) {
        let seq = self.async_seq.fetch_add(1, Ordering::SeqCst);
        let id = format!("hook-{}-{seq}", std::process::id());
        let running = RunningHook {
            id: id.clone(),
            event: event.to_owned(),
            tool: tool.map(str::to_owned),
            command: hook.command.clone(),
            started: Instant::now(),
        };
        self.async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(running);
        let this = self.clone();
        let hook = hook.clone();
        let event = event.to_owned();
        let tool = tool.map(str::to_owned);
        self.runtime.spawn(async move {
            let output = this
                .execute(&hook, &stdin_json, &event, tool.as_deref())
                .await;
            this.deliver_async(&hook, &event, output).await;
            this.finish_async(&id);
        });
    }

    /// Removes one finished async hook from the running registry and wakes waiters.
    fn finish_async(&self, id: &str) {
        self.async_running
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|hook| hook.id != id);
        self.async_signal.notify_waiters();
    }

    /// Delivers one finished async hook's result (ADR 0045 §5). Async hooks cannot change
    /// the call or the turn: their control fields are ignored with one warning. Exit 0
    /// `additionalContext` is an `inject`; exit 2 is an `inject` unless `asyncRewake` asks
    /// for a `followup` wake; every other end is one warning line (the run record is
    /// already in place). Nothing ever fails closed: the action the hook checked already
    /// happened.
    async fn deliver_async(
        &self,
        hook: &HookCommand,
        event: &str,
        output: Result<ProcOutput, HookError>,
    ) {
        let output = match output {
            Ok(output) => output,
            Err(error) => {
                (self.warn)(&format!("the {event} async hook failed: {error}"));
                return;
            }
        };
        match output.code {
            Some(0) => {
                let Some(parsed) = self.parsed_output(&output.stdout, event) else {
                    return;
                };
                let specific = parsed.hookSpecificOutput;
                let ignored = specific.as_ref().is_some_and(|specific| {
                    specific.permissionDecision.is_some()
                        || specific.updatedInput.is_some()
                        || specific.updatedToolOutput.is_some()
                        || specific.decision.is_some()
                }) || parsed.decision.is_some();
                if ignored {
                    (self.warn)(&format!(
                        "the {event} async hook returned permissionDecision, updatedInput, \
                         updatedToolOutput or decision; an async hook cannot change the call \
                         or the turn, so they are ignored"
                    ));
                }
                if let Some(context) = specific.and_then(|specific| specific.additionalContext) {
                    self.inject_async(format!("[{event} hook] {}", short(&context)))
                        .await;
                }
            }
            Some(2) => {
                let reason = short(&output.stderr);
                let reason = if reason.trim().is_empty() {
                    format!("the {event} hook exited with code 2")
                } else {
                    reason
                };
                if hook.async_rewake {
                    self.followup_async(reason).await;
                } else {
                    self.inject_async(reason).await;
                }
            }
            code => {
                (self.warn)(&format!(
                    "the {event} async hook exited with code {}: {}",
                    code.map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                    short(&output.stderr),
                ));
            }
        }
    }

    /// An async result waits for the next model call without opening a turn (ADR 0045 §5):
    /// the `inject` lane never wakes a driver, whether it is idle or busy when the hook
    /// finishes.
    async fn inject_async(&self, text: String) {
        if let Err(error) = self
            .inbox
            .inject(Message::user(text), InputSource::System)
            .await
        {
            (self.warn)(&format!("could not deliver an async hook result: {error}"));
        }
    }

    /// An `asyncRewake` exit 2 takes the `followup` lane through the same wake gate as a
    /// `Stop` continuation (ADR 0045 §5): inside the gate it counts one wake; past it the
    /// result degrades to a marked `inject` with the wake-budget notice (the journal
    /// `degraded` mark, one TUI notice, herdr `wake budget`).
    async fn followup_async(&self, text: String) {
        let message = Message::user(text);
        if self.gate.wake_allowed() {
            match self.inbox.followup(message, InputSource::System).await {
                Ok(_) => {
                    self.gate.note_wake();
                    self.degraded.store(0, Ordering::SeqCst);
                }
                Err(error) => {
                    (self.warn)(&format!("could not wake for an async hook result: {error}"));
                }
            }
        } else if self
            .inbox
            .inject_degraded(message, InputSource::System)
            .await
            .is_ok()
        {
            let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
            (self.wake)(&crate::job_notify::wake_budget_line(degraded));
        }
    }

    /// Spawns the hook with piped stdio: the sandboxed `/bin/sh -c` when the slot holds a
    /// sandbox (the same construction and environment rules the `shell` tool uses), a plain
    /// `/bin/sh -c` with the host environment otherwise. The child leads its own session
    /// (like `shell`), so a timeout can kill the whole tree and it never borrows the
    /// host's controlling terminal.
    fn spawn_piped(&self, hook: &HookCommand) -> std::io::Result<std::process::Child> {
        // The child leads its own session (like `shell`'s `setsid`): no controlling
        // terminal is inherited, and a timeout can signal the whole tree at once.
        fn own_session(command: &mut std::process::Command) {
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            #[cfg(not(unix))]
            {
                let _ = command;
            }
        }
        match self.sandbox.get() {
            Some(sandbox) => {
                let mut command = match &hook.args {
                    // Exec form (ADR 0045 §4): the program, with its arguments, never
                    // through a shell. The sandbox builds it exactly like `shell`.
                    Some(args) => {
                        let args: Vec<&str> = args.iter().map(String::as_str).collect();
                        sandbox
                            .command_str(&hook.command, &args)
                            .map_err(std::io::Error::other)?
                    }
                    None => sandbox
                        .command_str("/bin/sh", &["-c", &hook.command])
                        .map_err(std::io::Error::other)?,
                };
                command.current_dir(&self.workspace);
                if !sandbox.mode().is_enforced() {
                    // The transparent backend does not filter the environment. A hook's
                    // stdin carries model-controlled tool input, so the host's API keys
                    // stay out the same way they do for `shell`.
                    command.env_clear();
                    for name in &sandbox.policy().allow_env {
                        if let Some(value) = std::env::var_os(name) {
                            command.env(name, value);
                        }
                    }
                }
                own_session(&mut command);
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
            None => {
                let mut command = match &hook.args {
                    // Exec form (ADR 0045 §4): the program starts directly, inheriting the
                    // host environment like the `/bin/sh -c` path does.
                    Some(args) => {
                        let mut command = std::process::Command::new(&hook.command);
                        command.args(args);
                        command
                    }
                    None => {
                        let mut command = std::process::Command::new("/bin/sh");
                        command.arg("-c").arg(&hook.command);
                        command
                    }
                };
                own_session(&mut command);
                command
                    .current_dir(&self.workspace)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
            }
        }
    }

    /// One `PreToolUse` hook's verdict. `Err` is a hook error (fail closed: Core denies the
    /// call with `hook_error`).
    async fn run_pre_hook(
        &self,
        hook: &HookCommand,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<PreVerdict, HookError> {
        let stdin = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": tool,
            "tool_input": input,
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
        });
        let output = self.execute(hook, &stdin, "PreToolUse", Some(tool)).await?;
        match output.code {
            Some(2) => {
                let reason = short(&output.stderr);
                Ok(PreVerdict::Deny(if reason.trim().is_empty() {
                    "the hook blocked the call with no message".to_owned()
                } else {
                    reason
                }))
            }
            Some(0) => {
                let Some(parsed) = self.parsed_output(&output.stdout, "PreToolUse") else {
                    return Ok(PreVerdict::Continue);
                };
                let specific = parsed.hookSpecificOutput.unwrap_or_default();
                match specific.permissionDecision.as_deref() {
                    Some("deny") => {
                        let reason = specific
                            .reason
                            .or(parsed.reason)
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or_else(|| "the hook denied the call".to_owned());
                        Ok(PreVerdict::Deny(reason))
                    }
                    // A hook can never widen permissions (ADR 0012 P1, Codex parity): the
                    // decision is refused with a warning and treated as no decision. The
                    // call still follows the permission rules below.
                    Some("allow") | Some("ask") => {
                        let decision = specific.permissionDecision.unwrap_or_default();
                        (self.warn)(&format!(
                            "PreToolUse hook returned permissionDecision \"{decision}\", which is \
                             refused: a hook cannot widen permissions; the call still follows \
                             the permission rules"
                        ));
                        self.updated_input(specific.updatedInput, "PreToolUse")
                    }
                    Some(other) => {
                        (self.warn)(&format!(
                            "PreToolUse hook returned unknown permissionDecision \"{other}\"; ignoring it"
                        ));
                        self.updated_input(specific.updatedInput, "PreToolUse")
                    }
                    None => self.updated_input(specific.updatedInput, "PreToolUse"),
                }
            }
            _ => Err(HookError::new(format!(
                "the PreToolUse hook exited with code {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                short(&output.stderr),
            ))),
        }
    }

    /// The `updatedInput` half of an exit-0 verdict: an object replaces the tool input
    /// (threaded into the following hooks), anything else shaped is a hook error.
    fn updated_input(
        &self,
        updated: Option<serde_json::Value>,
        event: &str,
    ) -> Result<PreVerdict, HookError> {
        match updated {
            None | Some(serde_json::Value::Null) => Ok(PreVerdict::Continue),
            Some(serde_json::Value::Object(_)) => {
                Ok(PreVerdict::Updated(updated.expect("matched as an object")))
            }
            Some(_) => Err(HookError::new(format!(
                "the {event} hook returned a non-object updatedInput"
            ))),
        }
    }

    /// Applies one exit-0 `PostToolUse` output to the result. An `updatedToolOutput` string
    /// replaces the content first (one text part; `is_error` and `error_code` stay), then a
    /// `decision: "block"` reason is appended as the error feedback, then
    /// `additionalContext` as non-error feedback. A non-string `updatedToolOutput` is a hook
    /// error (fail closed).
    fn apply_post_output(
        &self,
        parsed: HookOutput,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        let specific = parsed.hookSpecificOutput.unwrap_or_default();
        if specific.permissionDecision.is_some() {
            (self.warn)(
                "PostToolUse hook returned permissionDecision, which only PreToolUse uses; \
                 ignoring it",
            );
        }
        if let Some(updated) = specific.updatedToolOutput {
            match updated {
                Some(serde_json::Value::String(text)) => {
                    result.content = vec![ContentPart::text(short(&text))];
                }
                // A present `null` lands here too: `null` is a value, not an omitted field,
                // and it is not a string.
                _ => {
                    return Err(HookError::new(
                        "the PostToolUse hook returned a non-string updatedToolOutput",
                    ));
                }
            }
        }
        if parsed.decision.as_deref() == Some("block") {
            let reason = specific.reason.or(parsed.reason).unwrap_or_default();
            let reason = if reason.trim().is_empty() {
                "the PostToolUse hook reported an error with no message".to_owned()
            } else {
                short(&reason)
            };
            result
                .content
                .push(ContentPart::text(format!("[PostToolUse hook] {reason}")));
            result.is_error = true;
        }
        if let Some(context) = specific.additionalContext {
            result.content.push(ContentPart::text(format!(
                "[PostToolUse hook] {}",
                short(&context)
            )));
        }
        Ok(())
    }

    /// The reasons one exit-0 `Stop` output contributes to the turn's single continuation:
    /// `decision: "block"` first, then `additionalContext` (ADR 0045 §2). Fields only
    /// `PreToolUse` uses warn once and are ignored.
    fn stop_output_reasons(&self, parsed: HookOutput) -> Vec<String> {
        let mut reasons = Vec::new();
        let specific = parsed.hookSpecificOutput.unwrap_or_default();
        if specific.permissionDecision.is_some() || specific.updatedInput.is_some() {
            (self.warn)(
                "Stop hook returned permissionDecision or updatedInput, which only PreToolUse \
                 uses; ignoring them",
            );
        }
        if parsed.decision.as_deref() == Some("block") {
            let reason = specific.reason.or(parsed.reason).unwrap_or_default();
            reasons.push(if reason.trim().is_empty() {
                "the Stop hook requested another turn".to_owned()
            } else {
                short(&reason)
            });
        }
        if let Some(context) = specific.additionalContext {
            reasons.push(short(&context));
        }
        reasons
    }

    /// Parses an exit-0 stdout as JSON per Claude: empty output continues silently, garbage
    /// warns and continues (the hook exited successfully, so there is nothing to fail
    /// closed on).
    fn parsed_output(&self, stdout: &str, event: &str) -> Option<HookOutput> {
        if stdout.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<HookOutput>(stdout) {
            Ok(parsed) => Some(parsed),
            Err(_) => {
                (self.warn)(&format!(
                    "the {event} hook exited 0 but its stdout was not JSON; ignoring it"
                ));
                None
            }
        }
    }

    /// One `PermissionRequest` hook's verdict (ADR 0045 §4). Unlike the other events, an
    /// exit-0 stdout that is not JSON fails closed: a script that was meant to answer the
    /// question and misspoke must not fall through to a prompt.
    ///
    /// `updatedPermissions` is read only for a layer that decides `allow`: an item
    /// `{ "type": "addRules", "destination": "session", "behavior": "allow" }`
    /// asks for a session grant, exactly like the approval prompt's "allow for this
    /// session" (ADR 0045 §4). The item's `rules` are not parsed. Any other `type`,
    /// `destination` or `behavior` warns one line and records nothing, and so does a
    /// present value that is not an array (a JSON `null` included: it is present, so it
    /// is not the same as the field's absence).
    fn session_grant_requested(&self, updated: Option<&Option<serde_json::Value>>) -> bool {
        let Some(updated) = updated else {
            return false;
        };
        let Some(items) = updated.as_ref().and_then(serde_json::Value::as_array) else {
            (self.warn)(
                "the PermissionRequest hook returned updatedPermissions that is not an \
                 array; ignoring it",
            );
            return false;
        };
        let mut granted = false;
        for item in items {
            let accepted = item.as_object().is_some_and(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("addRules")
                    && item.get("destination").and_then(serde_json::Value::as_str)
                        == Some("session")
                    && item.get("behavior").and_then(serde_json::Value::as_str) == Some("allow")
            });
            if accepted {
                granted = true;
            } else {
                (self.warn)(
                    "the PermissionRequest hook returned an updatedPermissions item that \
                     is ignored: only { \"type\": \"addRules\", \"destination\": \"session\", \
                     \"behavior\": \"allow\" } is supported",
                );
            }
        }
        granted
    }

    async fn run_permission_request_hook(
        &self,
        hook: &HookCommand,
        tool: &str,
        input: &serde_json::Value,
    ) -> Result<PermissionVerdict, HookError> {
        let stdin = serde_json::json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": tool,
            "tool_input": input,
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
        });
        let output = self
            .execute(hook, &stdin, "PermissionRequest", Some(tool))
            .await?;
        match output.code {
            Some(2) => {
                let reason = short(&output.stderr);
                Ok(PermissionVerdict::Deny(Some(if reason.trim().is_empty() {
                    "the hook blocked the call with no message".to_owned()
                } else {
                    reason
                })))
            }
            Some(0) => {
                if output.stdout.trim().is_empty() {
                    return Ok(PermissionVerdict::NoDecision);
                }
                let parsed: HookOutput = serde_json::from_str(&output.stdout).map_err(|_| {
                    HookError::new(
                        "the PermissionRequest hook exited 0 but its stdout was not JSON; \
                             the call is denied",
                    )
                })?;
                let Some(decision) = parsed
                    .hookSpecificOutput
                    .and_then(|specific| specific.decision)
                else {
                    // No decision: `updatedPermissions` alone contributes nothing (items
                    // count only from a layer that decides `allow`).
                    return Ok(PermissionVerdict::NoDecision);
                };
                if decision.updatedInput.is_some() || decision.interrupt.is_some() {
                    (self.warn)(
                        "PermissionRequest hook returned updatedInput or interrupt, which this \
                         version does not support; ignoring them",
                    );
                }
                match decision.behavior.as_deref() {
                    Some("allow") => Ok(PermissionVerdict::Allow {
                        session_grant: self
                            .session_grant_requested(decision.updatedPermissions.as_ref()),
                    }),
                    // Bounded like stderr: hook output enters the model's context.
                    Some("deny") => Ok(PermissionVerdict::Deny(
                        decision.message.map(|message| short(&message)),
                    )),
                    Some(other) => {
                        (self.warn)(&format!(
                            "PermissionRequest hook returned unknown decision.behavior \
                             \"{other}\"; ignoring it"
                        ));
                        Ok(PermissionVerdict::NoDecision)
                    }
                    None => {
                        (self.warn)(
                            "PermissionRequest hook returned a decision without behavior; \
                             ignoring it",
                        );
                        Ok(PermissionVerdict::NoDecision)
                    }
                }
            }
            _ => Err(HookError::new(format!(
                "the PermissionRequest hook exited with code {}: {}",
                output
                    .code
                    .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                short(&output.stderr),
            ))),
        }
    }
}

/// One finished hook process.
struct ProcOutput {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// One `PreToolUse` hook's verdict.
enum PreVerdict {
    Continue,
    Updated(serde_json::Value),
    Deny(String),
}

/// One `PermissionRequest` hook's verdict (ADR 0045 §4).
#[derive(Debug, PartialEq, Eq)]
enum PermissionVerdict {
    NoDecision,
    /// `session_grant` is the one supported `updatedPermissions` item: this call also
    /// records a session grant, like the approval prompt's "allow for this session".
    Allow {
        session_grant: bool,
    },
    Deny(Option<String>),
}

/// An exit-0 hook stdout, Claude's field names (kept verbatim, hence non-snake-case).
#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
struct HookOutput {
    #[serde(default)]
    hookSpecificOutput: Option<HookSpecificOutput>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    decision: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
struct HookSpecificOutput {
    #[serde(default)]
    permissionDecision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    updatedInput: Option<serde_json::Value>,
    #[serde(default)]
    additionalContext: Option<String>,
    /// `PermissionRequest` only: `decision { behavior, message, … }`. Every other event
    /// ignores the field.
    #[serde(default)]
    decision: Option<PermissionDecisionOutput>,
    /// `Option<Option<_>>` with [`present`]: `None` when the field is absent, `Some(None)`
    /// for a present JSON `null`, `Some(Some(value))` otherwise. The other option fields
    /// here may treat `null` as absence; only this one must fail closed on every
    /// non-string value.
    #[serde(default, deserialize_with = "present")]
    updatedToolOutput: Option<Option<serde_json::Value>>,
}

/// One `PermissionRequest` `hookSpecificOutput.decision` (ADR 0045 §4). `updatedPermissions`
/// is read only for the one item shape this version acts on; it is kept as
/// `Option<Option<_>>` (see [`present`]) so a present `null` warns once instead of reading
/// as an absent field.
#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
struct PermissionDecisionOutput {
    #[serde(default)]
    behavior: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Parsed by [`CommandHooks::session_grant_requested`], so a malformed value warns
    /// instead of failing the whole decision closed.
    #[serde(default, deserialize_with = "present")]
    updatedPermissions: Option<Option<serde_json::Value>>,
    #[serde(default, deserialize_with = "present")]
    updatedInput: Option<Option<serde_json::Value>>,
    #[serde(default, deserialize_with = "present")]
    interrupt: Option<Option<serde_json::Value>>,
}

/// Deserializes a possibly-`null` field while keeping "present" apart from "absent"
/// ([`HookSpecificOutput::updatedToolOutput`],
/// [`PermissionDecisionOutput::updatedPermissions`]): a plain `Option` maps a present JSON
/// `null` to `None`, so the field's own validation never runs. `T::deserialize` here is
/// `Option<Value>`, so `null` becomes `Some(None)` and any value `Some(Some(value))`.
fn present<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Kills a hook child's whole session group when dropped. `spawn_piped` puts the child in its
/// own group (`setsid`), and the future that waits on it can be dropped — a cancelled turn
/// drops it at an `await` — so a kill that only runs on the timeout path would leak a hook
/// process nobody waits for. `kill_tree` is best effort; the spawn-blocking waiter still
/// reaps the child.
struct KillGroupOnDrop(Option<u32>);

impl KillGroupOnDrop {
    fn new(pid: u32) -> Self {
        Self(Some(pid))
    }

    /// The run ended normally (or the caller kills it itself): never touch the group again.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0.take() {
            kill_tree(pid);
        }
    }
}

/// Kills a timed-out hook's whole session group (`setsid` in `spawn_piped` put it in one).
/// Best effort, like the shell tool's own kill paths: the waiter still reaps the child.
fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // Negative pid = the group. The leader may already have been reaped (a hook that
        // forked and exited), so an error here is not a failure.
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Compiles one event's two layers in run order: the project entries first, then the
/// user's (ADR 0045 slice B). An invalid pattern warns once here (not per call) and the
/// entry never runs.
fn compile_layered(
    project: &[HookMatcher],
    user: &[HookMatcher],
    event: &str,
    warn: &HooksWarning,
) -> Vec<CompiledEntry> {
    project
        .iter()
        .chain(user.iter())
        .enumerate()
        .map(|(index, entry)| compile_entry(entry, event, index, warn))
        .collect()
}

/// Compiles one `{ matcher, hooks }` entry: the regex is built once at load time, so an
/// invalid pattern warns once there (not per call) and the entry never runs.
fn compile_entry(
    entry: &HookMatcher,
    event: &str,
    index: usize,
    warn: &HooksWarning,
) -> CompiledEntry {
    let matcher = match &entry.matcher {
        None => CompiledMatcher::All,
        Some(pattern) if pattern.trim().is_empty() || pattern.trim() == "*" => CompiledMatcher::All,
        Some(pattern) => match regex::Regex::new(&format!("^(?:{pattern})$")) {
            Ok(regex) => CompiledMatcher::Regex(regex),
            Err(error) => {
                (warn)(&format!(
                    "{event}[{index}] matcher {pattern:?} is not a valid regex ({error}); \
                     the entry will never run"
                ));
                CompiledMatcher::Never
            }
        },
    };
    CompiledEntry {
        hooks: entry.hooks.clone(),
        matcher,
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

/// Bounds hook stderr before it enters the transcript.
fn short(text: &str) -> String {
    let kept: String = text.chars().take(REASON_CHAR_LIMIT).collect();
    if kept.len() < text.len() {
        format!("{kept}…[truncated]")
    } else {
        kept
    }
}

impl PermissionRequestHooks for CommandHooks {
    /// Runs the `PermissionRequest` chain (ADR 0045 §4): project entries first, the user's
    /// after; any `deny` stops it; one `allow` stands unless a later hook denies; the same
    /// `command` + `args` runs once. `Err` is a hook error and denies the call with
    /// `hook_error`.
    fn permission_request<'a>(
        &'a self,
        call: &'a ToolCall,
    ) -> futures_util::future::BoxFuture<'a, Result<PermissionRequestDecision, HookError>> {
        Box::pin(async move {
            if self.permission_request.is_empty() {
                return Ok(PermissionRequestDecision::NoDecision);
            }
            // One run per command string, like the other events (ADR 0045 slice B).
            let mut seen = std::collections::HashSet::new();
            let mut allowed = false;
            let mut session_grant = false;
            for entry in &self.permission_request {
                if !Self::matches(&entry.matcher, &call.name) {
                    continue;
                }
                for hook in &entry.hooks {
                    if !seen.insert(dedup_key(hook)) {
                        continue;
                    }
                    match self
                        .run_permission_request_hook(hook, &call.name, &call.arguments)
                        .await?
                    {
                        PermissionVerdict::NoDecision => {}
                        PermissionVerdict::Allow {
                            session_grant: granted,
                        } => {
                            allowed = true;
                            // Items count only when the final decision is an allow; a
                            // later `deny` returns before this ever leaves the chain.
                            session_grant |= granted;
                        }
                        // A deny only narrows: the rest of the chain never runs.
                        PermissionVerdict::Deny(reason) => {
                            return Ok(PermissionRequestDecision::Deny { reason });
                        }
                    }
                }
            }
            Ok(if allowed {
                PermissionRequestDecision::Allow { session_grant }
            } else {
                PermissionRequestDecision::NoDecision
            })
        })
    }
}

impl Hooks for CommandHooks {
    async fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> Result<ToolDecision, HookError> {
        if self.pre.is_empty() {
            return Ok(ToolDecision::Allow);
        }
        let mut input = call.arguments.clone();
        let mut changed = false;
        // One run per command string: both layers may name the same hook, and the
        // second naming is skipped (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.pre {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                match self.run_pre_hook(hook, &call.name, &input).await? {
                    PreVerdict::Continue => {}
                    PreVerdict::Updated(next) => {
                        input = next;
                        changed = true;
                    }
                    PreVerdict::Deny(reason) => return Ok(ToolDecision::Deny { reason }),
                }
            }
        }
        if changed {
            // Re-validated by Core; hooks are not re-run (ADR 0012 P3).
            Ok(ToolDecision::Rewrite { arguments: input })
        } else {
            Ok(ToolDecision::Allow)
        }
    }

    async fn after_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
        result: &mut ToolResult,
    ) -> Result<(), HookError> {
        if self.post.is_empty() {
            return Ok(());
        }
        // One run per command string, like `PreToolUse` (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.post {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
                if !seen.insert(dedup_key(hook)) {
                    continue;
                }
                let stdin = serde_json::json!({
                    "hook_event_name": "PostToolUse",
                    "tool_name": call.name,
                    "tool_input": call.arguments,
                    "tool_response": serde_json::to_value(&result).unwrap_or(serde_json::Value::Null),
                    "cwd": self.workspace.to_string_lossy(),
                    "session_id": self.session_id,
                });
                // An async hook starts the same way and the result continues at once
                // (ADR 0045 §5): its output is delivered through `inject`/`followup` later.
                if hook.is_async() {
                    self.spawn_async(hook, stdin, "PostToolUse", Some(&call.name));
                    continue;
                }
                let output = self
                    .execute(hook, &stdin, "PostToolUse", Some(&call.name))
                    .await?;
                match output.code {
                    Some(0) => {
                        if let Some(parsed) = self.parsed_output(&output.stdout, "PostToolUse") {
                            self.apply_post_output(parsed, result)?;
                        }
                    }
                    // The reason is fed back to the model with the result: the tool already
                    // ran, so there is nothing to deny — the feedback rides along instead.
                    Some(2) => {
                        let reason = short(&output.stderr);
                        let reason = if reason.trim().is_empty() {
                            "the PostToolUse hook reported an error with no message".to_owned()
                        } else {
                            reason
                        };
                        result
                            .content
                            .push(ContentPart::text(format!("[PostToolUse hook] {reason}")));
                        result.is_error = true;
                    }
                    _ => {
                        return Err(HookError::new(format!(
                            "the PostToolUse hook exited with code {}: {}",
                            output
                                .code
                                .map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                            short(&output.stderr),
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// A completed turn runs the Stop hooks once, after `RunFinished`, plus the built-in
    /// intent-stop validator (which runs even with no `Stop` hook configured); any other
    /// terminal status (cancelled, failed, budget, incomplete, refused, poisoned) runs
    /// nothing. A hook asking for more (exit 2) opens another turn through the inbox. The
    /// work spawns onto the session runtime so this sync callback never blocks a turn.
    fn on_turn_end(&self, _cx: &HookContext, outcome: &RunOutcome) {
        // Read the turn's provenance here, synchronously, at its own boundary: the value
        // belongs to this turn, so a `Stop` run for an earlier turn (still in flight) can
        // never consume it and a later claim can never overwrite it (ADR 0045 §2).
        let continuation = self.take_turn_continuation();
        if !matches!(outcome.status, RunStatus::Completed) {
            // A non-completed turn breaks the validator's chain (like a rule miss) and the
            // hook idle-continuation streak (the same reset point), ends any `Stop`
            // continuation it was opened by, and no Stop run will come for this turn: drop
            // the turn-start count.
            self.consecutive.store(0, Ordering::SeqCst);
            self.hook_idle.store(0, Ordering::SeqCst);
            // Saturating: sessions that never called `note_turn_started` (only the
            // hosts that read `pending_stops` wire it) must not wrap the counter.
            self.pending
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
            self.pending_signal.notify_one();
            return;
        }
        // The turn-start count covers the spawned run; it drops when the run ends.
        let this = self.clone();
        let outcome = outcome.clone();
        self.runtime.spawn(async move {
            this.run_stop_once_with(&outcome, continuation).await;
            // Paired with `note_turn_started`; saturating for the same reason.
            this.pending
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    Some(n.saturating_sub(1))
                })
                .ok();
            this.pending_signal.notify_one();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_core::{CancelReason, CancellationToken};
    use std::sync::Mutex;

    use crate::wake_gate::{MAX_WAKES_PER_WINDOW, WAKE_WINDOW};

    static N: AtomicU32 = AtomicU32::new(0);

    /// A unique temp root, removed on drop. `ws` is the canonicalized workspace.
    struct TempRoot {
        dir: PathBuf,
        ws: PathBuf,
    }

    impl TempRoot {
        fn new(label: &str) -> Self {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir()
                .join(format!("radian-hooks-{}-{label}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let ws = dir.join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let dir = std::fs::canonicalize(&dir).unwrap();
            let ws = std::fs::canonicalize(&ws).unwrap();
            Self { dir, ws }
        }

        fn outside(&self, name: &str) -> PathBuf {
            self.dir.join(name)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    struct Fixture {
        _root: TempRoot,
        hooks: CommandHooks,
        warnings: Arc<Mutex<Vec<String>>>,
        wake_notices: Arc<Mutex<Vec<String>>>,
        intent_notices: Arc<Mutex<Vec<String>>>,
        hook_stop_notices: Arc<Mutex<Vec<String>>>,
        inbox: Arc<Inbox>,
    }

    fn warnings_collector() -> (HooksWarning, Arc<Mutex<Vec<String>>>) {
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let warn: HooksWarning = {
            let warnings = Arc::clone(&warnings);
            Arc::new(move |message: &str| {
                warnings.lock().unwrap().push(message.to_owned());
            })
        };
        (warn, warnings)
    }

    fn hook(command: &str) -> HookCommand {
        HookCommand {
            command: command.to_owned(),
            args: None,
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        }
    }

    fn entry(matcher: &str, commands: Vec<HookCommand>) -> HookMatcher {
        HookMatcher {
            matcher: Some(matcher.to_owned()),
            hooks: commands,
        }
    }

    /// An executable `/bin/sh` script for the exec-form tests. The shebang is what makes the
    /// kernel run it directly, with exactly the arguments it was given.
    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A `PreToolUse` config with one hook for the `shell` tool.
    fn pre_hook(hook: HookCommand) -> HooksConfig {
        HooksConfig {
            pre_tool_use: vec![entry("shell", vec![hook])],
            ..HooksConfig::default()
        }
    }

    fn fixture_with(config: &HooksConfig, sandbox: SandboxSlot) -> Fixture {
        layered_with_root(
            TempRoot::new("case"),
            config,
            &HooksConfig::default(),
            sandbox,
        )
    }

    fn layered(user: &HooksConfig, project: &HooksConfig) -> Fixture {
        layered_with_root(
            TempRoot::new("layered"),
            user,
            project,
            SandboxSlot::empty(),
        )
    }

    fn layered_with_root(
        root: TempRoot,
        user: &HooksConfig,
        project: &HooksConfig,
        sandbox: SandboxSlot,
    ) -> Fixture {
        let (warn, warnings) = warnings_collector();
        let (wake, wake_notices) = warnings_collector();
        let (intent, intent_notices) = warnings_collector();
        let (hook_stop, hook_stop_notices) = warnings_collector();
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            user,
            project,
            &root.ws,
            "test-session",
            sandbox,
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
            intent,
            hook_stop,
        );
        Fixture {
            _root: root,
            hooks,
            warnings,
            wake_notices,
            intent_notices,
            hook_stop_notices,
            inbox,
        }
    }

    fn plain(config: &HooksConfig) -> Fixture {
        fixture_with(config, SandboxSlot::empty())
    }

    fn pre_config(matcher: &str, command: &str) -> HooksConfig {
        HooksConfig {
            pre_tool_use: vec![entry(matcher, vec![hook(command)])],
            ..HooksConfig::default()
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
        }
    }

    fn cx() -> HookContext {
        HookContext {
            cancellation: CancellationToken::new(),
            model_calls: 0,
        }
    }

    /// A completed turn whose last assistant message is `text` (or none).
    fn completed_outcome(status: RunStatus, text: Option<&str>) -> RunOutcome {
        RunOutcome {
            status,
            final_message: text.map(Message::assistant),
            unapplied_steering: Vec::new(),
            model_calls: 0,
        }
    }

    /// A completed turn with no last assistant message: the validator stays off, so
    /// hook-only tests keep their old shape.
    fn completed_quiet() -> RunOutcome {
        completed_outcome(RunStatus::Completed, None)
    }

    /// A completed turn ending with `text`.
    fn completed_text(text: &str) -> RunOutcome {
        completed_outcome(RunStatus::Completed, Some(text))
    }

    /// A completed turn that made `model_calls` model requests: `1` means the model only
    /// spoke (no tool round trip), and anything above means at least one tool call.
    fn completed_calls(text: Option<&str>, model_calls: u32) -> RunOutcome {
        let mut outcome = completed_outcome(RunStatus::Completed, text);
        outcome.model_calls = model_calls;
        outcome
    }

    /// One hook continuation cycle at a turn end and the turn it opened, claimed so the
    /// next `run_stop_once` sees it as a hook-opened turn. Returns whether a turn opened.
    async fn hook_round(fixture: &Fixture, outcome: &RunOutcome) -> bool {
        fixture.hooks.run_stop_once(outcome).await;
        fixture.inbox.claim_next_turn().await.is_some()
    }

    fn warned(warnings: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        warnings.lock().unwrap().clone()
    }

    /// Exit 2 denies the call with the hook's stderr as the reason.
    #[tokio::test]
    async fn pre_tool_use_exit_2_denies_with_the_stderr_reason() {
        let fixture = plain(&pre_config("shell", "echo blocked-by-policy >&2; exit 2"));
        let decision = fixture
            .hooks
            .before_tool_call(
                &cx(),
                &call("shell", serde_json::json!({"command": "rm -rf /"})),
            )
            .await
            .unwrap();
        match decision {
            ToolDecision::Deny { reason } => assert!(
                reason.contains("blocked-by-policy"),
                "the stderr is the reason: {reason}"
            ),
            other => panic!("exit 2 must deny, got {other:?}"),
        }
    }

    /// `hookSpecificOutput.permissionDecision: "deny"` denies, with its reason.
    #[tokio::test]
    async fn permission_decision_deny_denies() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny","reason":"custom-deny"}}'"#;
        let fixture = plain(&pre_config("write", script));
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("write", serde_json::json!({"path": "a"})))
            .await
            .unwrap();
        match decision {
            ToolDecision::Deny { reason } => {
                assert!(reason.contains("custom-deny"), "{reason}")
            }
            other => panic!("deny must deny, got {other:?}"),
        }
    }

    /// `"allow"` (and `"ask"`) are refused with a warning and treated as no decision: the
    /// hook-level verdict stays `Allow`, and the permission rules below still decide.
    #[tokio::test]
    async fn allow_is_refused_and_treated_as_no_decision() {
        for decision in ["allow", "ask"] {
            let script = format!(
                r#"printf '%s' '{{"hookSpecificOutput":{{"permissionDecision":"{decision}"}}}}'"#
            );
            let fixture = plain(&pre_config("shell", &script));
            let verdict = fixture
                .hooks
                .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
                .await
                .unwrap();
            assert!(
                matches!(verdict, ToolDecision::Allow),
                "{decision} must not decide: {verdict:?}"
            );
            assert!(
                warned(&fixture.warnings)
                    .iter()
                    .any(|warning| warning.contains("refused") && warning.contains(decision)),
                "{decision} must warn: {:?}",
                warned(&fixture.warnings)
            );
        }
    }

    /// `updatedInput` rewrites the call's arguments (re-validated, hooks not re-run).
    #[tokio::test]
    async fn updated_input_rewrites_the_call() {
        let script =
            r#"printf '%s' '{"hookSpecificOutput":{"updatedInput":{"command":"echo safe"}}}'"#;
        let fixture = plain(&pre_config("shell", script));
        let decision = fixture
            .hooks
            .before_tool_call(
                &cx(),
                &call("shell", serde_json::json!({"command": "echo evil"})),
            )
            .await
            .unwrap();
        match decision {
            ToolDecision::Rewrite { arguments } => assert_eq!(
                arguments,
                serde_json::json!({"command": "echo safe"}),
                "the hook input replaces the arguments"
            ),
            other => panic!("updatedInput must rewrite, got {other:?}"),
        }
    }

    /// A non-object `updatedInput` is a hook error: failing closed beats running a call
    /// with arguments the hook malformed.
    #[tokio::test]
    async fn non_object_updated_input_is_a_hook_error() {
        let script =
            r#"printf '%s' '{"hookSpecificOutput":{"updatedInput":["not","an","object"]}}'"#;
        let fixture = plain(&pre_config("shell", script));
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("updatedInput"), "{error}");
    }

    /// PostToolUse exit 2 feeds the hook's stderr back to the model with the result.
    #[tokio::test]
    async fn post_tool_use_exit_2_feeds_the_reason_back() {
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook("echo lint-broke >&2; exit 2")])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert!(result.is_error, "feedback is an error signal");
        assert!(result.text().contains("patched"), "the result stays");
        assert!(
            result.text().contains("lint-broke"),
            "the reason rides along: {}",
            result.text()
        );
    }

    /// Edge case 1: exit-0 `additionalContext` rides back with the result and does **not**
    /// mark it an error. Not reading stdout (the old PostToolUse behavior) leaves the result
    /// as the tool wrote it and fails this.
    #[tokio::test]
    async fn post_tool_use_additional_context_feeds_back_without_an_error() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"reformatted"}}'"#;
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(script)])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "additionalContext is not an error signal: {:?}",
            result
        );
        assert_eq!(
            result.text(),
            "patched[PostToolUse hook] reformatted",
            "the feedback appends to the result"
        );
    }

    /// Edge case 2: `decision: "block"` with a reason is the JSON form of exit 2: the same
    /// result text and the same error mark. Ignoring the exit-0 object leaves the result
    /// untouched and fails the equality.
    #[tokio::test]
    async fn post_tool_use_block_decision_matches_exit_2() {
        let config = HooksConfig {
            post_tool_use: vec![entry(
                "patch",
                vec![hook("printf '%s' 'lint-broke' >&2; exit 2")],
            )],
            ..HooksConfig::default()
        };
        let exit_2 = plain(&config);
        let mut from_exit = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        exit_2
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut from_exit)
            .await
            .unwrap();

        let script = r#"printf '%s' '{"decision":"block","reason":"lint-broke"}'"#;
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(script)])],
            ..HooksConfig::default()
        };
        let block = plain(&config);
        let mut from_block = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        block
            .hooks
            .after_tool_call(
                &cx(),
                &call("patch", serde_json::json!({})),
                &mut from_block,
            )
            .await
            .unwrap();

        assert!(from_exit.is_error && from_block.is_error);
        assert_eq!(
            from_block.text(),
            from_exit.text(),
            "block and exit 2 carry the same feedback"
        );
    }

    /// Edge case 3 (unit half): the project layer's `updatedToolOutput` replaces first, so
    /// the user layer sees the replaced result on stdin and its own replacement wins.
    /// Ignoring `updatedToolOutput` leaves the user's stdin and the final result at the
    /// tool's original text and fails both.
    #[tokio::test]
    async fn the_user_hook_sees_the_project_replaced_result() {
        let root = TempRoot::new("post-replace");
        let stdin_path = root.dir.join("user-stdin.json");
        let project = HooksConfig {
            post_tool_use: vec![entry(
                "patch",
                vec![hook(
                    r#"printf '%s' '{"hookSpecificOutput":{"updatedToolOutput":"from-project"}}'"#,
                )],
            )],
            ..HooksConfig::default()
        };
        let user_script = format!(
            "input=$(cat); printf '%s' \"$input\" > {}; \
             printf '%s' '{{\"hookSpecificOutput\":{{\"updatedToolOutput\":\"from-user\"}}}}'",
            stdin_path.display()
        );
        let user = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(&user_script)])],
            ..HooksConfig::default()
        };
        let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        let seen = stdin["tool_response"].to_string();
        assert!(
            seen.contains("from-project"),
            "the user hook sees the project replacement: {seen}"
        );
        assert_eq!(
            result.text(),
            "from-user",
            "the user layer's replacement is the final result"
        );
        assert!(
            stdin["tool_response"]["is_error"] == serde_json::json!(false),
            "the replacement keeps is_error: {stdin}"
        );
    }

    /// Edge case 4: an `updatedToolOutput` that is not a string fails closed as a hook
    /// error, like a non-zero exit. Treating it as text or ignoring it returns `Ok` here.
    #[tokio::test]
    async fn a_non_string_updated_tool_output_is_a_hook_error() {
        for value in ["42", "{\"body\":\"no\"}"] {
            let script = format!(
                r#"printf '%s' '{{"hookSpecificOutput":{{"updatedToolOutput":{value}}}}}'"#
            );
            let config = HooksConfig {
                post_tool_use: vec![entry("patch", vec![hook(&script)])],
                ..HooksConfig::default()
            };
            let fixture = plain(&config);
            let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
            let error = fixture
                .hooks
                .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("updatedToolOutput"), "{error}");
        }
    }

    /// Rework 2, item 1: a present `updatedToolOutput` of JSON `null` is a value, not an
    /// absent field, and `null` is not a string, so it fails closed like any other
    /// non-string. Deserializing the field into a plain `Option<Value>` conflates `null`
    /// with absence and leaves the result unchanged instead.
    #[tokio::test]
    async fn a_null_updated_tool_output_is_a_hook_error() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"updatedToolOutput":null}}'"#;
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(script)])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        let error = fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("updatedToolOutput"), "{error}");
        assert_eq!(result.text(), "patched", "the result is not replaced");
    }

    /// Edge case 7: a 10,000-character `additionalContext` keeps 2,000 characters and the
    /// existing truncation marker. Forwarding it whole fails the length and the marker.
    #[tokio::test]
    async fn a_long_additional_context_is_truncated() {
        let long = "x".repeat(10_000);
        let script =
            format!(r#"printf '%s' '{{"hookSpecificOutput":{{"additionalContext":"{long}"}}}}'"#);
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(&script)])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert_eq!(
            result.text(),
            format!(
                "patched[PostToolUse hook] {}…[truncated]",
                "x".repeat(REASON_CHAR_LIMIT)
            )
        );
    }

    /// Edge case 8: exit-0 stdout that is not JSON warns once and leaves the result alone
    /// (the old PostToolUse never read stdout, so there was no warning and this failed).
    #[tokio::test]
    async fn a_non_json_post_tool_use_stdout_warns_once_and_leaves_the_result_alone() {
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook("printf '%s' 'not json'")])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert_eq!(result.text(), "patched");
        assert!(!result.is_error);
        let warnings = warned(&fixture.warnings);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("not JSON"), "{warnings:?}");
    }

    /// `permissionDecision` on `PostToolUse` and `permissionDecision`/`updatedInput` on
    /// `Stop` are ignored with one warning line and change nothing.
    #[tokio::test]
    async fn fields_only_pre_tool_use_uses_warn_once_and_are_ignored() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
        let config = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook(script)])],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert_eq!(result.text(), "patched");
        assert_eq!(
            warned(&fixture.warnings).len(),
            1,
            "{:?}",
            warned(&fixture.warnings)
        );

        let script = r#"printf '%s' '{"hookSpecificOutput":{"permissionDecision":"deny","updatedInput":{"command":"x"}}}'"#;
        let fixture = plain(&stop_config(script));
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the ignored fields open no turn"
        );
        assert_eq!(
            warned(&fixture.warnings).len(),
            1,
            "{:?}",
            warned(&fixture.warnings)
        );
    }

    /// A hook that overruns its timeout fails closed.
    #[tokio::test]
    async fn a_hook_timeout_fails_closed() {
        let mut config = pre_config("shell", "sleep 30");
        config.pre_tool_use[0].hooks[0].timeout_secs = Some(1);
        let fixture = plain(&config);
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    /// A matcher that does not match never runs the hook (an `exit 2` hook on another
    /// tool leaves this call alone).
    #[tokio::test]
    async fn a_non_matching_matcher_does_not_run_the_hook() {
        let fixture = plain(&pre_config("shell", "exit 2"));
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("read", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "an unmatched hook must not decide: {decision:?}"
        );
    }

    /// `*` matches every tool.
    #[tokio::test]
    async fn star_matcher_matches_every_tool() {
        let fixture = plain(&pre_config("*", "exit 2"));
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "`*` must match `shell`: {decision:?}"
        );
    }

    /// A regex matcher matches the tool it describes.
    #[tokio::test]
    async fn regex_matcher_matches_the_tool_it_describes() {
        let fixture = plain(&pre_config("wri.*", "exit 2"));
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("write", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Deny { .. }),
            "`wri.*` must match `write`: {decision:?}"
        );
    }

    /// The match is anchored to the whole tool name: `sh` does not match `shell`.
    #[tokio::test]
    async fn an_unanchored_matcher_does_not_match_a_prefix() {
        let fixture = plain(&pre_config("sh", "exit 2"));
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "`sh` must not match `shell` (anchored): {decision:?}"
        );
    }

    /// An invalid pattern warns once at load time and the hook never runs.
    #[tokio::test]
    async fn an_invalid_matcher_warns_once_and_never_runs() {
        let config = pre_config("shell(", "exit 2");
        let fixture = plain(&config);
        assert!(
            warned(&fixture.warnings)
                .iter()
                .any(|warning| warning.contains("not a valid regex") && warning.contains("shell(")),
            "an invalid pattern must warn at load: {:?}",
            warned(&fixture.warnings)
        );
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(
            matches!(decision, ToolDecision::Allow),
            "an invalid matcher must never run: {decision:?}"
        );
    }

    /// Claude's stdin field names reach the hook: event, tool, input, cwd, session id.
    #[tokio::test]
    async fn the_hook_sees_claudes_stdin_fields() {
        let root = TempRoot::new("stdin");
        let stdin_path = root.dir.join("stdin.json");
        let config = pre_config("shell", &format!("cat > {}", stdin_path.display()));
        let (warn, _) = warnings_collector();
        let (wake, _) = warnings_collector();
        let (intent, _) = warnings_collector();
        let (hook_stop, _) = warnings_collector();
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            &config,
            &HooksConfig::default(),
            &root.ws,
            "sid-9",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
            intent,
            hook_stop,
        );
        hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({"command": "hi"})))
            .await
            .unwrap();
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        assert_eq!(stdin["hook_event_name"], "PreToolUse");
        assert_eq!(stdin["tool_name"], "shell");
        assert_eq!(stdin["tool_input"]["command"], "hi");
        assert_eq!(stdin["session_id"], "sid-9");
        assert_eq!(stdin["cwd"], root.ws.to_string_lossy().as_ref());
    }

    /// Edge case 1: arguments reach the program exactly as written. `/bin/sh` would split
    /// `a b`, expand `$(…)` and treat `;`/`*`; the exec form must do none of that, and no
    /// file may be created by the ``$(touch …)`` argument.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_args_reach_the_program_verbatim_without_a_shell() {
        let root = TempRoot::new("exec-args");
        let out = root.ws.join("args.txt");
        let script = root.ws.join("capture");
        write_script(
            &script,
            &format!("printf '%s\\n' \"$@\" > {}", out.display()),
        );
        let marker = root.outside("expanded");
        let touch = format!("$(touch {})", marker.display());
        let config = pre_hook(HookCommand {
            command: script.display().to_string(),
            args: Some(vec![
                "a b".to_owned(),
                touch.clone(),
                ";".to_owned(),
                "*".to_owned(),
            ]),
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        });
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(
            seen.lines().collect::<Vec<_>>(),
            vec!["a b", touch.as_str(), ";", "*"],
            "arguments arrive verbatim, with no word splitting or expansion"
        );
        assert!(!marker.exists(), "$(...) must not run");
    }

    /// Edge case 2: an exec-form hook in the `workspace` tier runs inside the sandbox, so a
    /// write outside the workspace fails and lands nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn exec_args_run_inside_the_workspace_sandbox() {
        let root = TempRoot::new("sandbox-exec-args");
        let outside = root.outside("escaped.txt");
        let script = root.ws.join("escape");
        write_script(
            &script,
            &format!("echo escaped > {}; exit 1", outside.display()),
        );
        let config = pre_hook(HookCommand {
            command: script.display().to_string(),
            args: Some(vec!["ignored".to_owned()]),
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        });
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
                .expect("the workspace sandbox probes clean on this machine");
        let fixture = layered_with_root(
            root,
            &config,
            &HooksConfig::default(),
            SandboxSlot::with(sandbox),
        );
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("exited with code"),
            "the sandbox denial is a hook error: {error}"
        );
        assert!(
            !outside.exists(),
            "the write outside the workspace must not land"
        );
    }

    /// Edge case 4: dedup keys on `command` plus `args` (ADR 0045 §1 item 3). The same
    /// program with different arguments runs twice, and the same program with the same
    /// arguments written in both layers runs once. A command-only key fails both halves.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_dedup_key_is_command_plus_args() {
        let root = TempRoot::new("dedup-args");
        let log = root.ws.join("runs.txt");
        let script = root.ws.join("log");
        write_script(&script, &format!("echo \"$1\" >> {}", log.display()));
        let hook = |arg: &str| HookCommand {
            command: script.display().to_string(),
            args: Some(vec![arg.to_owned()]),
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        };
        let project = pre_hook(hook("one"));
        let user = HooksConfig {
            pre_tool_use: vec![entry("shell", vec![hook("one"), hook("two")])],
            ..HooksConfig::default()
        };
        let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
        fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        let seen = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            seen.lines().collect::<Vec<_>>(),
            vec!["one", "two"],
            "the duplicate (command, args) runs once; the other args still run"
        );
    }

    /// Edge case 5: a command that does not exist is a hook error (fail closed, unchanged)
    /// and is recorded as `failed to start`.
    #[tokio::test]
    async fn a_missing_exec_command_fails_to_start_and_is_recorded() {
        let root = TempRoot::new("missing-exec");
        let config = pre_hook(HookCommand {
            command: root.ws.join("does-not-exist").display().to_string(),
            args: Some(Vec::new()),
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        });
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("could not start"),
            "the spawn failure stays a hook error: {error}"
        );
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].end, HookRunEnd::FailedToStart);
        assert_eq!(runs[0].event, "PreToolUse");
        assert_eq!(runs[0].tool.as_deref(), Some("shell"));
        assert!(
            runs[0].line().contains("failed to start"),
            "{}",
            runs[0].line()
        );
    }

    /// Edge case 6: a timed-out hook is recorded as `timed out`, and its milliseconds are at
    /// least the timeout.
    #[tokio::test]
    async fn a_timed_out_hook_is_recorded_as_timed_out() {
        let root = TempRoot::new("timed-out-record");
        let config = pre_hook(HookCommand {
            command: "/bin/sleep".to_owned(),
            args: Some(vec!["5".to_owned()]),
            timeout_secs: Some(1),
            async_hook: false,
            async_rewake: false,
        });
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].end, HookRunEnd::TimedOut);
        assert!(
            runs[0].millis >= 1000,
            "the timeout's record must be at least 1000ms: {}",
            runs[0].millis
        );
    }

    /// A run that ends on a signal has no exit code; the closed set calls that `cancelled`.
    #[tokio::test]
    async fn a_hook_killed_by_a_signal_is_recorded_cancelled() {
        let root = TempRoot::new("cancelled-record");
        let config = pre_hook(HookCommand {
            command: "/bin/sh".to_owned(),
            args: Some(vec!["-c".to_owned(), "kill -KILL $$".to_owned()]),
            timeout_secs: None,
            async_hook: false,
            async_rewake: false,
        });
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("signal"), "{error}");
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs[0].end, HookRunEnd::Cancelled);
    }

    /// Edge case 5's record shape, plus `Stop`'s missing tool name: the agreed one-line
    /// record is `<event>:<tool> [<command first line>] <end> (<n>ms)`. A command shaped
    /// `echo first` records only `echo first` even when the shell command has more lines.
    #[tokio::test]
    async fn every_run_records_the_agreed_line() {
        let root = TempRoot::new("record-line");
        let config = pre_config("shell", "true");
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].end, HookRunEnd::Status(0));
        assert!(
            runs[0]
                .line()
                .starts_with("PreToolUse:shell [true] status 0 ("),
            "{}",
            runs[0].line()
        );
        assert!(runs[0].line().ends_with("ms)"), "{}", runs[0].line());
        assert!(runs[0].millis >= 1, "a run never reads 0ms");

        let root = TempRoot::new("record-line-stop");
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("true\nexit 0")],
            }],
            ..HooksConfig::default()
        };
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!(
            runs[0].line().starts_with("Stop [true] status 0 ("),
            "Stop has no tool name, and only the command's first line shows: {}",
            runs[0].line()
        );
    }

    /// Edge case 7: 60 runs keep the last 50, newest first.
    #[tokio::test]
    async fn the_run_ring_keeps_the_last_50_newest_first() {
        let fixture = plain(&HooksConfig::default());
        for n in 0..60 {
            fixture.hooks.record_run(
                "Stop",
                None,
                &format!("cmd-{n}"),
                HookRunEnd::Status(0),
                Instant::now(),
            );
        }
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), HOOK_RUN_HISTORY, "the ring holds 50");
        assert_eq!(runs[0].command, "cmd-59", "newest first");
        assert_eq!(runs[49].command, "cmd-10", "the oldest kept is #10");
    }

    /// `workspace` runs the hook sandboxed: a hook writing outside the workspace fails
    /// (fail closed), and nothing lands outside. The trailing `exit 1` pins the
    /// verdict to the hook-error path on every shell: without it dash exits 2 on
    /// a failed redirection (a `Deny`) while bash exits 1 (a hook error).
    #[tokio::test]
    async fn workspace_runs_the_hook_sandboxed() {
        let root = TempRoot::new("sandbox");
        let outside = root.outside("escaped.txt");
        let config = pre_config(
            "shell",
            &format!("echo escaped > {}; exit 1", outside.display()),
        );
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
                .expect("the workspace sandbox probes clean on this machine");
        let fixture = layered_with_root(
            root,
            &config,
            &HooksConfig::default(),
            SandboxSlot::with(sandbox),
        );
        let error = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("exited with code"),
            "the sandbox denial is a hook error: {error}"
        );
        assert!(
            !outside.exists(),
            "the write outside the workspace must not land"
        );
    }

    /// The sandboxed hook can still write inside the workspace.
    #[tokio::test]
    async fn a_sandboxed_hook_writes_inside_the_workspace() {
        let root = TempRoot::new("sandbox-inside");
        let config = pre_config("shell", "echo ok > marker.txt");
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
                .expect("the workspace sandbox probes clean on this machine");
        let ws = root.ws.clone();
        let fixture = layered_with_root(
            root,
            &config,
            &HooksConfig::default(),
            SandboxSlot::with(sandbox),
        );
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
        assert!(ws.join("marker.txt").exists());
    }

    /// `Stop` exit 2 opens another turn with the stderr as a `System` followup. Continuations
    /// minutes apart never fill the rate gate: six in a row all open a turn; a fast loop
    /// fills the window, and past the gate the hook still runs but its verdict degrades to
    /// a marked inject (no wake) with the wake-budget notice, until the window passes.
    #[tokio::test]
    async fn stop_continuations_minutes_apart_are_never_gated() {
        tokio::time::pause();
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        for i in 0..6 {
            fixture.hooks.run_stop_once(&completed_quiet()).await;
            let entry = fixture
                .inbox
                .claim_next_turn()
                .await
                .unwrap_or_else(|| panic!("round {i}: the continuation opens a turn"));
            assert!(
                entry.message.text().contains("keep-going"),
                "the stderr is the input: {}",
                entry.message.text()
            );
            assert_eq!(entry.source, InputSource::System);
            tokio::time::advance(std::time::Duration::from_secs(180)).await;
        }
    }

    /// A fast `Stop` loop (every continuation ~1 s apart) fills the window: the 11th run
    /// still runs the hook, but its exit-2 verdict degrades to a marked inject (no wake)
    /// with the wake-budget notice; the window passing reopens followups with no user
    /// message.
    #[tokio::test]
    async fn stop_fast_loop_is_gated_after_n_and_recovers_after_the_window() {
        tokio::time::pause();
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        for i in 0..MAX_WAKES_PER_WINDOW {
            fixture.hooks.run_stop_once(&completed_quiet()).await;
            assert!(
                fixture.inbox.claim_next_turn().await.is_some(),
                "continuation {i}: still inside the budget"
            );
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        // The 11th run still runs the hook; its exit-2 verdict degrades to a marked
        // inject (no wake) with the wake-budget notice.
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the gated continuation must not open a turn"
        );
        let entry = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the gated hook injects the verdict");
        assert!(
            entry.message.text().contains("keep-going"),
            "the stderr is the input: {}",
            entry.message.text()
        );
        assert_eq!(fixture.hooks.degraded(), 1);
        assert_eq!(
            warned(&fixture.wake_notices).len(),
            1,
            "the degrade fires the wake notice: {:?}",
            warned(&fixture.wake_notices)
        );
        assert!(
            warned(&fixture.wake_notices)[0].starts_with("wake budget used up: 1 "),
            "the notice carries the degrade count: {:?}",
            warned(&fixture.wake_notices)
        );
        // The window passing alone reopens the gate: no user message needed; the later
        // wake ends the burst.
        tokio::time::advance(WAKE_WINDOW + std::time::Duration::from_secs(1)).await;
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert_eq!(fixture.hooks.stop_wakes(), 1);
        assert_eq!(
            fixture.hooks.degraded(),
            0,
            "the later wake ends the degrade burst"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "after the window: the continuation opens a turn again"
        );
    }

    /// A user message restarts the `Stop` budget at once.
    #[tokio::test]
    async fn stop_user_message_resets_the_budget() {
        tokio::time::pause();
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        for _ in 0..3 {
            fixture.hooks.run_stop_once(&completed_quiet()).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), 3);
        // A user message restarts the budget.
        fixture.hooks.reset_wakes();
        assert_eq!(fixture.hooks.degraded(), 0);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert_eq!(fixture.hooks.stop_wakes(), 1);
    }

    /// A cancelled turn runs no Stop hook: an exit-2 hook after a user interrupt must not
    /// open another turn.
    #[tokio::test]
    async fn stop_does_not_run_on_a_cancelled_turn() {
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        fixture
            .hooks
            .run_stop_once(&completed_outcome(
                RunStatus::Cancelled(CancelReason::Requested),
                Some("Now the docs-only ADR updates."),
            ))
            .await;
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "a cancelled turn starts no new turn"
        );
    }

    /// A failed turn runs no Stop hook either.
    #[tokio::test]
    async fn stop_does_not_run_on_a_failed_turn() {
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        fixture
            .hooks
            .run_stop_once(&completed_outcome(
                RunStatus::Failed,
                Some("Now the docs-only ADR updates."),
            ))
            .await;
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "a failed turn starts no new turn"
        );
    }

    /// `Stop` exit 0 ends the turn with no followup.
    #[tokio::test]
    async fn stop_exit_0_continues_nothing() {
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("exit 0")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "exit 0 continues nothing"
        );
    }

    /// Edge case 5: a project-layer `additionalContext` and a user-layer exit 2 join one
    /// continuation: one turn, one wake, one message carrying both reasons (project first).
    /// Ignoring `additionalContext` loses the project reason and fails.
    #[tokio::test]
    async fn stop_additional_context_merges_with_another_layers_exit_2() {
        let project = stop_config(
            r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"project-context"}}'"#,
        );
        let user = stop_config("echo user-reason >&2; exit 2");
        let fixture = layered(&user, &project);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the merged continuation opens a turn");
        let text = entry.message.text();
        let project_at = text
            .find("project-context")
            .expect("project reason: {text}");
        let user_at = text.find("user-reason").expect("user reason: {text}");
        assert!(
            project_at < user_at,
            "the project reason rides first: {text}"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "one continuation, not two"
        );
    }

    /// `decision: "block"` on `Stop` opens the same continuation as exit 2 (the JSON form).
    #[tokio::test]
    async fn stop_block_decision_opens_the_same_continuation() {
        let config = stop_config(r#"printf '%s' '{"decision":"block","reason":"keep-going"}'"#);
        let fixture = plain(&config);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("block opens a turn");
        assert!(entry.message.text().contains("keep-going"));
        assert_eq!(fixture.hooks.stop_wakes(), 1);
    }

    /// Edge case 6: `additionalContext` past the wake gate degrades to a marked `inject`
    /// with the same three visible signals as exit 2 (inbox entry, degrade count, wake
    /// notice). Ignoring the field opens no continuation and injects nothing, so the
    /// step claim fails.
    #[tokio::test]
    async fn stop_additional_context_degrades_past_the_wake_gate() {
        tokio::time::pause();
        let config = stop_config(
            r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"keep-going"}}'"#,
        );
        let fixture = plain(&config);
        for i in 0..MAX_WAKES_PER_WINDOW {
            fixture.hooks.run_stop_once(&completed_quiet()).await;
            assert!(
                fixture.inbox.claim_next_turn().await.is_some(),
                "continuation {i}: still inside the budget"
            );
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the gated continuation must not open a turn"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the gated verdict injects");
        assert!(step.message.text().contains("keep-going"));
        assert_eq!(fixture.hooks.degraded(), 1);
        assert_eq!(
            warned(&fixture.wake_notices).len(),
            1,
            "the degrade fires the wake notice: {:?}",
            warned(&fixture.wake_notices)
        );
    }

    /// A `Stop` continuation past the gate runs the hook but injects the verdict with
    /// the degrade mark: the durable `InboxEnqueued` says `degraded`, so the journal leg
    /// matches the job notifier's (ADR 0045 §2).
    #[tokio::test]
    async fn stop_past_the_cap_injects_the_verdict_with_the_degrade_mark() {
        use radian_session::{EventData, Session, SessionCreatedData, SessionJournal};
        let root = TempRoot::new("stop-degraded");
        let journal = Arc::new(
            SessionJournal::create(
                root.dir.join("s.jsonl"),
                "sid",
                SessionCreatedData::new(
                    "/work/example",
                    serde_json::json!({"provider": "example", "model": "m-1"}),
                ),
            )
            .unwrap(),
        );
        let inbox = Inbox::durable(64, Arc::clone(&journal));
        let (warn, _) = warnings_collector();
        let (wake, wake_notices) = warnings_collector();
        let (intent, _) = warnings_collector();
        let (hook_stop, _) = warnings_collector();
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let hooks = CommandHooks::new(
            &config,
            &HooksConfig::default(),
            &root.ws,
            "test-session",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
            intent,
            hook_stop,
        );
        for _ in 0..MAX_WAKES_PER_WINDOW {
            hooks.run_stop_once(&completed_quiet()).await;
        }
        hooks.run_stop_once(&completed_quiet()).await;
        assert_eq!(hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        assert_eq!(hooks.degraded(), 1);

        // Waking followups record no degrade; the gated inject does.
        let session = Session::open(root.dir.join("s.jsonl")).unwrap();
        let degraded: Vec<bool> = session
            .events()
            .iter()
            .filter_map(|event| match &event.data {
                EventData::InboxEnqueued { degraded, .. } => Some(*degraded),
                _ => None,
            })
            .collect();
        assert_eq!(
            degraded,
            [vec![false; MAX_WAKES_PER_WINDOW as usize], vec![true]].concat()
        );

        // The wake notice names the one waiting continuation.
        let notices = wake_notices.lock().unwrap().clone();
        assert_eq!(
            notices,
            vec![
                "wake budget used up: 1 job notices are waiting — send any message to continue"
                    .to_owned()
            ]
        );
    }

    fn stop_config(command: &str) -> HooksConfig {
        HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook(command)],
            }],
            ..HooksConfig::default()
        }
    }

    /// Slice B: the project layer runs first, so the user's hook receives the arguments the
    /// project hook rewrote — and the call rewrites to exactly that. Swapping the layer
    /// order fails this: the user hook would see the original input.
    #[tokio::test]
    async fn the_project_layer_runs_first_and_threads_updated_input_into_the_user_hook() {
        let root = TempRoot::new("layer-order");
        let marker = root.dir.join("project-ran");
        let user_stdin = root.dir.join("user-stdin.json");
        let project = pre_config(
            "shell",
            &format!(
                "echo ran >> {}; printf '%s' '{{\"hookSpecificOutput\":{{\"updatedInput\":{{\"command\":\"from-project\"}}}}}}'",
                marker.display()
            ),
        );
        let user = pre_config("shell", &format!("cat > {}", user_stdin.display()));
        let fixture = layered(&user, &project);
        let decision = fixture
            .hooks
            .before_tool_call(
                &cx(),
                &call("shell", serde_json::json!({"command": "orig"})),
            )
            .await
            .unwrap();
        assert!(marker.exists(), "the project hook runs");
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&user_stdin).unwrap()).unwrap();
        assert_eq!(
            stdin["tool_input"],
            serde_json::json!({"command": "from-project"}),
            "the user hook sees the project-rewritten arguments: {stdin}"
        );
        match decision {
            ToolDecision::Rewrite { arguments } => {
                assert_eq!(arguments, serde_json::json!({"command": "from-project"}),)
            }
            other => panic!("the rewrite stands: {other:?}"),
        }
        let _ = root;
    }

    /// Slice B: a command string identical in both layers runs once per event. Dropping
    /// the dedupe runs it twice and fails the count.
    #[tokio::test]
    async fn an_identical_command_in_both_layers_runs_once() {
        let root = TempRoot::new("dedupe");
        let counter = root.dir.join("runs");
        let script = format!("echo ran >> {}; exit 0", counter.display());
        let project = pre_config("shell", &script);
        let user = pre_config("shell", &script);
        let fixture = layered(&user, &project);
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        assert!(matches!(decision, ToolDecision::Allow), "{decision:?}");
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1,
            "one run for two identical namings"
        );
    }

    /// Slice B: a deny from any layer stops the rest — a project deny never reaches the
    /// user's hooks. Letting later hooks run after a deny touches the marker and fails.
    #[tokio::test]
    async fn a_project_deny_stops_the_user_hooks() {
        let root = TempRoot::new("deny-stops");
        let marker = root.dir.join("user-ran");
        let project = pre_config("shell", "echo denied-by-project >&2; exit 2");
        let user = pre_config("shell", &format!("touch {}", marker.display()));
        let fixture = layered(&user, &project);
        let decision = fixture
            .hooks
            .before_tool_call(&cx(), &call("shell", serde_json::json!({})))
            .await
            .unwrap();
        match decision {
            ToolDecision::Deny { reason } => assert!(
                reason.contains("denied-by-project"),
                "the project reason stands: {reason}"
            ),
            other => panic!("the deny stands: {other:?}"),
        }
        assert!(
            !marker.exists(),
            "a deny stops the later hooks: the user hook must not run"
        );
        let _ = root;
    }

    /// Slice B: two `Stop` exit-2 verdicts open a single continuation with both reasons
    /// joined in one message, counted as one wake. Opening two turns (or two wakes)
    /// fails the claim and the count.
    #[tokio::test]
    async fn two_stop_exit_2_verdicts_open_one_continuation_with_both_reasons() {
        let project = stop_config("echo project-reason >&2; exit 2");
        let user = stop_config("echo user-reason >&2; exit 2");
        let fixture = layered(&user, &project);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the one continuation opens a turn");
        assert!(
            entry.message.text().contains("project-reason"),
            "the project reason rides along: {}",
            entry.message.text()
        );
        assert!(
            entry.message.text().contains("user-reason"),
            "the user reason joins it in the same message: {}",
            entry.message.text()
        );
        assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "no second continuation opens"
        );
    }

    /// Slice B: identical `Stop` commands run once: one turn, one wake, one execution.
    #[tokio::test]
    async fn an_identical_stop_command_runs_once() {
        let root = TempRoot::new("stop-dedupe");
        let counter = root.dir.join("runs");
        let script = format!(
            "echo ran >> {}; echo same-reason >&2; exit 2",
            counter.display()
        );
        let project = stop_config(&script);
        let user = stop_config(&script);
        let (warn, _) = warnings_collector();
        let (wake, _) = warnings_collector();
        let (intent, _) = warnings_collector();
        let (hook_stop, _) = warnings_collector();
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            &user,
            &project,
            &root.ws,
            "test-session",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
            intent,
            hook_stop,
        );
        hooks.run_stop_once(&completed_quiet()).await;
        let entry = inbox
            .claim_next_turn()
            .await
            .expect("the one continuation opens a turn");
        assert_eq!(
            entry.message.text().matches("same-reason").count(),
            1,
            "the reason appears once: {}",
            entry.message.text()
        );
        assert_eq!(hooks.stop_wakes(), 1);
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1,
            "one execution for two identical namings"
        );
    }

    /// Slice B (project check-after-edit): without a project `PostToolUse` an edit starts
    /// no hook — the result passes through untouched.
    #[tokio::test]
    async fn without_a_project_post_tool_use_an_edit_starts_no_hook() {
        let project = HooksConfig {
            post_tool_use: vec![entry("shell", vec![hook("echo no-edit >&2; exit 2")])],
            ..HooksConfig::default()
        };
        let fixture = layered(&HooksConfig::default(), &project);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.text(), "patched");
    }

    /// Slice B (project check-after-edit): a project `PostToolUse` on an edit runs
    /// synchronously, once per edit, its feedback riding directly after the edit's result —
    /// before the user's own feedback. Swapping the layers fails the order.
    #[tokio::test]
    async fn a_project_post_tool_use_runs_once_per_edit_with_its_feedback_first() {
        let project = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook("echo project-lint >&2; exit 2")])],
            ..HooksConfig::default()
        };
        let user = HooksConfig {
            post_tool_use: vec![entry("patch", vec![hook("echo user-lint >&2; exit 2")])],
            ..HooksConfig::default()
        };
        let fixture = layered(&user, &project);
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(&cx(), &call("patch", serde_json::json!({})), &mut result)
            .await
            .unwrap();
        assert!(result.is_error, "feedback is an error signal");
        let text = result.text();
        assert!(text.contains("patched"), "the result stays: {text}");
        let project_at = text.find("project-lint").expect("project feedback: {text}");
        let user_at = text.find("user-lint").expect("user feedback: {text}");
        assert!(
            project_at < user_at,
            "the project feedback rides first: {text}"
        );
    }

    /// The built-in validator fires with no hook configured at all: one `System`
    /// followup carrying the fixed text and the `intent-stop` origin, counted as one
    /// wake. Keeping the old early return (no Stop hook means no run) leaves the inbox
    /// empty and fails this.
    #[tokio::test]
    async fn intent_stop_fires_with_no_hook_configured() {
        let fixture = plain(&HooksConfig::default());
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the validator opens a turn");
        assert_eq!(entry.source, InputSource::System);
        assert_eq!(entry.message.text(), intent_stop::INTENT_STOP_TEXT);
        assert!(
            intent_stop::is_intent_stop_continuation(&entry.message),
            "validator-driven followups carry the origin"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake");
        assert_eq!(fixture.hooks.consecutive_hits(), 1);
    }

    /// Ordinary ends hold the validator: a done report, a question, a long report whose
    /// last sentence matches, and a hit text with the switch off. Removing the rule
    /// (always firing) opens a turn for each and fails this.
    #[tokio::test]
    async fn ordinary_text_does_not_fire() {
        for text in [
            "Done.",
            "Is it done?",
            &format!("{}\n\nNext: the reviewer decides.", "x".repeat(300)),
        ] {
            let fixture = plain(&HooksConfig::default());
            fixture.hooks.run_stop_once(&completed_text(text)).await;
            assert!(
                fixture.inbox.claim_next_turn().await.is_none(),
                "no continuation for {text:?}"
            );
            assert_eq!(fixture.hooks.stop_wakes(), 0);
            assert_eq!(fixture.hooks.consecutive_hits(), 0);
        }
        // The kill switch: a hit text with `[hooks] intent_stop = false` holds too.
        let user = HooksConfig {
            intent_stop: Some(false),
            ..HooksConfig::default()
        };
        let fixture = layered(&user, &HooksConfig::default());
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the switch turns the validator off"
        );
    }

    /// A waiting text holds while a background job runs (the model may be legitimately
    /// waiting on it), and fires once the registry is gone.
    #[tokio::test]
    async fn waiting_with_a_running_job_does_not_fire() {
        let fixture = plain(&HooksConfig::default());
        let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
        let sandbox = radian_sandbox::Sandbox::new(
            radian_sandbox::SandboxMode::WorkspaceWrite,
            &fixture._root.ws,
        )
        .expect("the workspace sandbox probes clean on this machine");
        let _started = jobs
            .start(&sandbox, &fixture._root.ws, "sleep 60")
            .expect("the job starts");
        fixture.hooks.set_jobs(Some(&jobs));
        assert_eq!(fixture.hooks.jobs_running(), 1);
        fixture
            .hooks
            .run_stop_once(&completed_text("Waiting for the completion notice."))
            .await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "a running job suppresses the validator"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
        jobs.kill_all();
        drop(jobs);
        fixture
            .hooks
            .run_stop_once(&completed_text("Waiting for the completion notice."))
            .await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "with no job the same text fires"
        );
    }

    /// A hook exit-2 and a validator hit join one continuation: one `followup`, one
    /// wake, the hook reasons first and the fixed text last, marked with the origin.
    /// Splitting the validator into its own followup opens two turns and fails this.
    #[tokio::test]
    async fn hook_and_validator_verdicts_join_one_continuation() {
        let config = stop_config("echo hook-reason >&2; exit 2");
        let fixture = plain(&config);
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the one continuation opens a turn");
        assert_eq!(entry.source, InputSource::System);
        let text = entry.message.text();
        assert!(
            text.contains("hook-reason"),
            "the hook reason rides: {text}"
        );
        assert!(
            text.contains(intent_stop::INTENT_STOP_TEXT),
            "the validator text joins it: {text}"
        );
        assert!(
            intent_stop::is_intent_stop_continuation(&entry.message),
            "the joint message carries the origin"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 1, "one wake, not two");
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "at most one continuation per turn"
        );
        assert_eq!(fixture.hooks.consecutive_hits(), 1);
    }

    /// Two continuations in a row, then the third consecutive hit reports `blocked`
    /// instead of continuing: no turn opens, one cap line fires, the verdict injects
    /// degraded (journal-visible, no wake), and the count stays saturated. A user
    /// message (`reset_wakes`) clears the chain. Dropping the cap opens the third turn
    /// and fails this.
    #[tokio::test]
    async fn third_consecutive_hit_reports_blocked_instead_of_continuing() {
        let fixture = plain(&HooksConfig::default());
        let hit = completed_text("Now the docs-only ADR updates.");
        for round in 0..2 {
            fixture.hooks.run_stop_once(&hit).await;
            assert!(
                fixture.inbox.claim_next_turn().await.is_some(),
                "round {round}: the continuation opens a turn"
            );
        }
        assert_eq!(fixture.hooks.consecutive_hits(), 2);
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the third hit opens no turn"
        );
        assert_eq!(
            warned(&fixture.intent_notices),
            vec![intent_stop::intent_stop_cap_line()],
            "exactly one cap notice"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the cap verdict injects degraded");
        assert!(
            intent_stop::is_intent_stop_continuation(&step.message),
            "the cap inject is journal-visible"
        );
        assert_eq!(fixture.hooks.degraded(), 1);
        assert_eq!(
            fixture.hooks.consecutive_hits(),
            2,
            "saturated, not cleared"
        );
        fixture.hooks.reset_wakes();
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "a user message clears the chain"
        );
    }

    /// A rule miss between two hits breaks the chain: the hit after the miss counts
    /// from zero, so the run below caps only at its own third consecutive hit.
    #[tokio::test]
    async fn a_rule_miss_breaks_the_chain() {
        let fixture = plain(&HooksConfig::default());
        let hit = completed_text("Now the docs-only ADR updates.");
        fixture.hooks.run_stop_once(&hit).await;
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        fixture.hooks.run_stop_once(&completed_text("Done.")).await;
        assert!(fixture.inbox.claim_next_turn().await.is_none());
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
        fixture.hooks.run_stop_once(&hit).await;
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        fixture.hooks.run_stop_once(&hit).await;
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the third consecutive hit after the miss caps"
        );
        assert_eq!(warned(&fixture.intent_notices).len(), 1);
    }

    /// While `/loop` is armed the turn end belongs to the loop: a hit text opens
    /// nothing and counts nothing; disarmed, the same hit fires. Removing the loop
    /// check fires the armed case and fails this.
    #[tokio::test]
    async fn intent_stop_stays_off_while_loop_is_armed() {
        let fixture = plain(&HooksConfig::default());
        let hit = completed_text("Now the docs-only ADR updates.");
        fixture.hooks.set_loop_active(true);
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "an armed loop owns the turn end"
        );
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
        fixture.hooks.set_loop_active(false);
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "disarmed, the same hit fires"
        );
    }

    /// An `Active` or `Paused` goal owns the turn end (it re-posts itself): a hit text
    /// opens nothing; a `Done` goal leaves the validator alone. Removing the goal
    /// check fires the active case and fails this.
    #[tokio::test]
    async fn intent_stop_stays_off_while_a_goal_owns_the_turn_end() {
        use radian_session::{GoalState, GoalStatus};

        let fixture = plain(&HooksConfig::default());
        let hit = completed_text("Now the docs-only ADR updates.");
        let share: crate::goal::SharedGoal = Arc::new(std::sync::Mutex::new(Some(
            GoalState::fresh("o".into(), GoalState::default_budget()),
        )));
        fixture.hooks.set_goal(Arc::clone(&share));
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "an active goal owns the turn end"
        );
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
        share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Paused;
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "a paused goal still owns the turn end"
        );
        share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Done;
        fixture.hooks.run_stop_once(&hit).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "a done goal leaves the validator alone"
        );
    }

    /// A non-completed turn breaks the validator's chain through `on_turn_end` (like a
    /// rule miss), and no Stop run comes for it: the pending count drops synchronously.
    #[tokio::test]
    async fn a_cancelled_turn_breaks_the_chain_with_no_stop_run() {
        let fixture = plain(&HooksConfig::default());
        let hit = completed_text("Now the docs-only ADR updates.");
        fixture.hooks.note_turn_started();
        fixture.hooks.run_stop_once(&hit).await;
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        assert_eq!(fixture.hooks.consecutive_hits(), 1);
        fixture.hooks.on_turn_end(
            &cx(),
            &completed_outcome(
                RunStatus::Cancelled(CancelReason::Requested),
                Some("Now x."),
            ),
        );
        assert_eq!(fixture.hooks.consecutive_hits(), 0);
        assert_eq!(fixture.hooks.pending_stops(), 0);
    }

    /// The validator's continuation counts against the wake gate: with the window full
    /// of hook-driven wakes, a hit degrades to a marked inject with the wake-budget
    /// notice instead of opening a turn. Not counting the gate opens the turn and fails
    /// this.
    #[tokio::test]
    async fn the_wake_gate_counts_validator_continuations() {
        tokio::time::pause();
        let config = stop_config("echo keep-going >&2; exit 2");
        let fixture = plain(&config);
        for _ in 0..MAX_WAKES_PER_WINDOW {
            fixture.hooks.run_stop_once(&completed_quiet()).await;
            assert!(fixture.inbox.claim_next_turn().await.is_some());
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "past the gate the validator degrades like a hook"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the gated verdict injects");
        assert!(step.message.text().contains(intent_stop::INTENT_STOP_TEXT));
        assert!(
            intent_stop::is_intent_stop_continuation(&step.message),
            "the degraded message keeps the origin"
        );
        assert_eq!(fixture.hooks.degraded(), 1);
        assert_eq!(
            warned(&fixture.wake_notices).len(),
            1,
            "the gate fires its own notice, not the cap line"
        );
        assert!(warned(&fixture.intent_notices).is_empty());
        assert_eq!(fixture.hooks.consecutive_hits(), 1, "the hit still counts");
    }

    /// `Stop` stdin carries `last_assistant_message` (Claude's field name): the joined
    /// text parts, or empty when the turn has none.
    #[tokio::test]
    async fn stop_stdin_carries_last_assistant_message() {
        let root = TempRoot::new("stop-stdin");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!("cat > {}", stdin_path.display()));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        assert_eq!(stdin["hook_event_name"], "Stop");
        assert_eq!(
            stdin["last_assistant_message"],
            "Now the docs-only ADR updates."
        );
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        assert_eq!(stdin["last_assistant_message"], "");
    }

    /// Runs one `Stop` cycle whose hook writes its stdin to `path`, and returns that JSON.
    /// Removes an earlier copy first, so a hook that never ran cannot pass a stale read
    /// off as this run's.
    async fn stop_stdin_at(
        fixture: &Fixture,
        path: &Path,
        outcome: &RunOutcome,
    ) -> serde_json::Value {
        let _ = std::fs::remove_file(path);
        fixture.hooks.run_stop_once(outcome).await;
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Releases a blocked `Stop` hook even when an assertion fails first, so a broken
    /// test fails instead of waiting for the running child. `disarm` is called once the
    /// hook is known to have exited; until then, a failure writes the release file and
    /// waits long enough for the blocked child to observe it before the fixture's temp
    /// directory (and the file) goes away.
    struct Release {
        path: PathBuf,
        armed: bool,
    }

    impl Release {
        fn new(path: PathBuf) -> Self {
            Self { path, armed: true }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for Release {
        fn drop(&mut self) {
            if self.armed {
                let _ = std::fs::write(&self.path, b"go");
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }

    /// Waits for a file a hook creates (polled: the hook is a child process).
    async fn wait_for_file(path: &Path) {
        for _ in 0..600 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} never appeared", path.display());
    }

    async fn wait_for_json(path: &Path) -> serde_json::Value {
        wait_for_file(path).await;
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Waits until a `Stop` continuation was actually queued (the gate records the wake
    /// after the enqueue lands).
    async fn wait_until_continuation(fixture: &Fixture) {
        for _ in 0..600 {
            if fixture.hooks.stop_wakes() > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the continuation was never queued");
    }

    /// A `Stop` run for an earlier turn finishes late (exit 2) while a user's turn B is
    /// open, and B ends after that verdict. B's own `Stop` must report `false`; only the
    /// continuation the late verdict queued may report `true` (ADR 0045 §2). A
    /// session-global pending flag is consumed by whichever turn ends first, so B reports
    /// `true` and the real continuation reports `false` there.
    #[tokio::test]
    async fn a_late_stop_verdict_does_not_leak_into_a_users_turn() {
        let root = TempRoot::new("stop-late-verdict");
        let a_stdin = root.dir.join("a.json");
        let b_stdin = root.dir.join("b.json");
        let c_stdin = root.dir.join("c.json");
        let started = root.dir.join("a-started");
        let release = root.dir.join("release");
        let script = format!(
            "stdin=$(cat); case \"$stdin\" in \
             *'\"last_assistant_message\":\"A\"'*) printf '%s' \"$stdin\" > '{a}'; \
             touch '{started}'; while [ ! -f '{release}' ]; do sleep 0.01; done; \
             echo keep-going >&2; exit 2 ;; \
             *'\"last_assistant_message\":\"B\"'*) printf '%s' \"$stdin\" > '{b}' ;; \
             *) printf '%s' \"$stdin\" > '{c}' ;; esac",
            a = a_stdin.display(),
            b = b_stdin.display(),
            c = c_stdin.display(),
            started = started.display(),
            release = release.display(),
        );
        let config = stop_config(&script);
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let mut release_guard = Release::new(release.clone());

        // Turn A ends; its Stop hook blocks before exiting 2.
        fixture.hooks.on_turn_end(&cx(), &completed_text("A"));
        wait_for_file(&started).await;

        // The user speaks while A's hook still runs: turn B opens and is not a
        // continuation.
        fixture.hooks.reset_wakes();
        fixture
            .inbox
            .followup("B", InputSource::User)
            .await
            .unwrap();
        let b = fixture.inbox.claim_next_turn().await.expect("B opens");
        assert_eq!(b.message.text(), "B");
        assert!(
            !fixture.hooks.stop_hook_active(),
            "a user message opens a turn that is not a continuation"
        );

        // A's hook now exits 2 and queues its continuation; B ends after that verdict.
        std::fs::write(&release, b"go").unwrap();
        wait_until_continuation(&fixture).await;
        release_guard.disarm();
        fixture.hooks.on_turn_end(&cx(), &completed_text("B"));
        let b_stdin = wait_for_json(&b_stdin).await;
        assert_eq!(
            b_stdin["stop_hook_active"],
            serde_json::json!(false),
            "the user's turn must not consume the other turn's continuation: {b_stdin}"
        );

        // Only the continuation the late verdict queued is one.
        fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the late verdict queued a continuation");
        assert!(
            fixture.hooks.stop_hook_active(),
            "claiming the continuation marks its turn"
        );
        fixture.hooks.on_turn_end(&cx(), &completed_text("C"));
        let c_stdin = wait_for_json(&c_stdin).await;
        assert_eq!(
            c_stdin["stop_hook_active"],
            serde_json::json!(true),
            "the continuation itself says so: {c_stdin}"
        );
    }

    /// Edge case 1: with no job, no `/loop` and no goal the three fields are present — not
    /// omitted — as `stop_hook_active: false`, `background_tasks: []`, `session_crons: []`.
    /// Inserting an array only when it is non-empty fails this.
    #[tokio::test]
    async fn stop_stdin_always_carries_the_three_new_fields() {
        let root = TempRoot::new("stop-extra-empty");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!("cat > {}", stdin_path.display()));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            stdin["stop_hook_active"],
            serde_json::json!(false),
            "{stdin}"
        );
        assert_eq!(stdin["background_tasks"], serde_json::json!([]), "{stdin}");
        assert_eq!(stdin["session_crons"], serde_json::json!([]), "{stdin}");
    }

    /// Edge case 2: a background job running at turn end is one `background_tasks` entry
    /// with its id, `type: "shell"`, `status: "running"`, the first line as `description`
    /// and the full multi-line command as `command`. Reporting the first-line summary as
    /// the command fails this.
    #[tokio::test]
    async fn stop_stdin_lists_a_running_background_job() {
        let root = TempRoot::new("stop-extra-job");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!("cat > {}", stdin_path.display()));
        let ws = root.ws.clone();
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
        let sandbox = radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::FullAccess, &ws)
            .expect("full access needs no backend");
        let started = jobs
            .start(&sandbox, &ws, "sleep 60\necho done")
            .expect("the job starts");
        fixture.hooks.set_jobs(Some(&jobs));
        let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        let tasks = stdin["background_tasks"]
            .as_array()
            .unwrap_or_else(|| panic!("an array: {stdin}"));
        assert_eq!(tasks.len(), 1, "{stdin}");
        assert_eq!(tasks[0]["id"], started.id.as_str(), "{stdin}");
        assert_eq!(tasks[0]["type"], "shell", "{stdin}");
        assert_eq!(tasks[0]["status"], "running", "{stdin}");
        assert_eq!(tasks[0]["description"], "sleep 60", "{stdin}");
        assert_eq!(tasks[0]["command"], "sleep 60\necho done", "{stdin}");
        jobs.kill_all();
    }

    /// Edge case 3: the turn a `Stop` exit-2 continuation opens reports
    /// `stop_hook_active: true`; the turn after it, which opens nothing, reports `false`.
    /// The provenance travels with the queued message: the claim is what makes the turn a
    /// continuation. Never setting it fails the middle assertion; never clearing it fails
    /// the last.
    #[tokio::test]
    async fn the_continued_turn_reports_stop_hook_active() {
        let root = TempRoot::new("stop-active");
        let stdin_path = root.dir.join("stdin.json");
        let marker = root.dir.join("continued");
        let config = stop_config(&format!(
            "cat > {stdin}; if [ -f {marker} ]; then exit 0; else touch {marker}; \
             echo keep-going >&2; exit 2; fi",
            stdin = stdin_path.display(),
            marker = marker.display(),
        ));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());

        let first = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            first["stop_hook_active"],
            serde_json::json!(false),
            "{first}"
        );
        assert!(
            !fixture.hooks.stop_hook_active(),
            "nothing is claimed yet, so no running turn is a continuation"
        );
        fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the exit 2 opens the continued turn");
        assert!(
            fixture.hooks.stop_hook_active(),
            "claiming the continuation message marks the turn it opens"
        );

        let second = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            second["stop_hook_active"],
            serde_json::json!(true),
            "the continued turn says so: {second}"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the exit 0 opens nothing"
        );

        let after = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            after["stop_hook_active"],
            serde_json::json!(false),
            "the turn after the continuation is not one: {after}"
        );
    }

    /// Edge case 4: a user message while a continuation is queued drops its provenance,
    /// so the turn that later claims it reports `false`. Dropping the pending-set clear
    /// from `reset_wakes` (the followup/steer/inject reset) fails this.
    #[tokio::test]
    async fn a_user_message_clears_stop_hook_active() {
        let root = TempRoot::new("stop-active-user");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!(
            "cat > {}; echo keep-going >&2; exit 2",
            stdin_path.display()
        ));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        // The same reset the three user-message lanes (followup, steer, inject) call,
        // before the queued continuation is claimed.
        fixture.hooks.reset_wakes();
        fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the exit 2 queued a continuation");
        assert!(
            !fixture.hooks.stop_hook_active(),
            "a user message ends the queued continuation"
        );
        let next = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            next["stop_hook_active"],
            serde_json::json!(false),
            "the continuation turn is no longer one: {next}"
        );
    }

    /// Edge case 5: a non-completed (cancelled) turn after a continuation clears the
    /// provenance; the next completed turn's `Stop` reports `false`. Keeping the claimed
    /// continuation's flag through a non-completed turn fails this.
    #[tokio::test]
    async fn a_cancelled_turn_clears_stop_hook_active() {
        let root = TempRoot::new("stop-active-cancel");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!(
            "cat > {}; echo keep-going >&2; exit 2",
            stdin_path.display()
        ));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("the exit 2 queued a continuation");
        assert!(
            fixture.hooks.stop_hook_active(),
            "the claimed continuation marked this turn"
        );
        fixture.hooks.on_turn_end(
            &cx(),
            &completed_outcome(
                RunStatus::Cancelled(CancelReason::Requested),
                Some("Now x."),
            ),
        );
        assert!(
            !fixture.hooks.stop_hook_active(),
            "a non-completed turn ends the continuation"
        );
        let next = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(next["stop_hook_active"], serde_json::json!(false), "{next}");
    }

    /// Edge case 6: `session_crons` lists an armed `/loop` and an `Active` or `Paused` goal
    /// with its status, and nothing for a `Done` or cleared goal. Reporting a paused goal as
    /// active (or keeping a done one) fails this.
    #[tokio::test]
    async fn session_crons_lists_the_loop_and_the_goal() {
        use radian_session::{GoalState, GoalStatus};

        let root = TempRoot::new("stop-crons");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!("cat > {}", stdin_path.display()));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let share: crate::goal::SharedGoal = Arc::new(std::sync::Mutex::new(Some(
            GoalState::fresh("ship it".into(), GoalState::default_budget()),
        )));
        fixture.hooks.set_goal(Arc::clone(&share));
        fixture.hooks.set_loop_active(true);

        let both = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            both["session_crons"],
            serde_json::json!([
                {"id": "loop", "type": "loop"},
                {"id": "goal", "type": "goal", "status": "active"},
            ]),
            "the loop and the active goal: {both}"
        );

        share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Paused;
        fixture.hooks.set_loop_active(false);
        let paused = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            paused["session_crons"],
            serde_json::json!([{"id": "goal", "type": "goal", "status": "paused"}]),
            "a paused goal is still a scheduler: {paused}"
        );

        share.lock().unwrap().as_mut().expect("a goal").status = GoalStatus::Done;
        let done = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            done["session_crons"],
            serde_json::json!([]),
            "a done goal adds nothing: {done}"
        );

        *share.lock().unwrap() = None;
        let cleared = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            cleared["session_crons"],
            serde_json::json!([]),
            "a cleared goal adds nothing: {cleared}"
        );
    }

    /// Edge case 7: a job registry that was dropped (the `Weak` no longer upgrades) reads
    /// as `background_tasks: []` and never panics. An `unwrap` on the upgrade panics here.
    #[tokio::test]
    async fn a_dropped_job_registry_reads_as_no_background_tasks() {
        let root = TempRoot::new("stop-extra-dropped");
        let stdin_path = root.dir.join("stdin.json");
        let config = stop_config(&format!("cat > {}", stdin_path.display()));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let jobs = Arc::new(radian_coding::Jobs::new(fixture._root.dir.join("jobs")));
        fixture.hooks.set_jobs(Some(&jobs));
        drop(jobs);
        let stdin = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            stdin["background_tasks"],
            serde_json::json!([]),
            "a dropped registry reads as no jobs: {stdin}"
        );
    }

    /// Edge case 1: eight hook continuations in a row with text-only turns. The 7th still
    /// opens a turn; the 8th does not — it injects marked degraded, fires one notice line,
    /// and counts one degraded item. Dropping the cap opens the 8th and fails the inbox
    /// assertions; dropping the notice fails the notice assertion.
    #[tokio::test]
    async fn eight_idle_hook_continuations_degrade_the_eighth_continuation() {
        let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
        for round in 0..7 {
            assert!(
                hook_round(&fixture, &completed_calls(None, 1)).await,
                "continuation {round} still opens a turn"
            );
        }
        assert_eq!(
            fixture.hooks.hook_idle_continuations(),
            6,
            "the first turn end is not a continuation; six idle continued turns preceded"
        );
        // The eighth continuation request (the seventh continued turn's Stop) takes the cap.
        fixture.hooks.run_stop_once(&completed_calls(None, 1)).await;
        assert_eq!(fixture.hooks.hook_idle_continuations(), 7);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the eighth continuation opens no turn"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the capped continuation injects the verdict");
        assert!(
            step.message.text().contains("keep-going"),
            "the reasons ride the inject: {}",
            step.message.text()
        );
        assert_eq!(fixture.hooks.degraded(), 1, "the inject is marked degraded");
        assert_eq!(
            warned(&fixture.hook_stop_notices),
            vec![hook_stop_cap_line()],
            "exactly one notice line"
        );
        assert!(
            warned(&fixture.wake_notices).is_empty(),
            "the idle cap is not the wake budget: {:?}",
            warned(&fixture.wake_notices)
        );
    }

    /// Edge case 2: seven idle hook continuations, then a continued turn that calls a tool,
    /// then seven more idle continuations — no degrade, because the tool call reset the
    /// counter. Keeping the count across the tool turn caps the later streak and fails the
    /// empty-notice assertion.
    #[tokio::test]
    async fn a_tool_calling_continued_turn_resets_the_idle_streak() {
        tokio::time::pause();
        let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
        for round in 0..7 {
            assert!(
                hook_round(&fixture, &completed_calls(None, 1)).await,
                "continuation {round} opens a turn"
            );
        }
        assert_eq!(fixture.hooks.hook_idle_continuations(), 6);
        // The next continued turn did real work: its Stop sees model_calls > 1, resets the
        // streak, and opens the next continuation.
        fixture
            .hooks
            .run_stop_once_with(&completed_calls(None, 2), Continuation::Hook)
            .await;
        assert_eq!(
            fixture.hooks.hook_idle_continuations(),
            0,
            "a tool call resets the streak"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "the tool turn's Stop still opens its continuation"
        );
        // The wake window is a hard cap independent of the idle streak; this test is about
        // the reset, so let the first streak's wakes age out before the second.
        tokio::time::advance(WAKE_WINDOW + std::time::Duration::from_secs(1)).await;
        // Seven more continuation requests open; none takes the cap.
        for round in 0..6 {
            assert!(
                hook_round(&fixture, &completed_calls(None, 1)).await,
                "post-reset continuation {round} opens a turn"
            );
        }
        assert_eq!(fixture.hooks.hook_idle_continuations(), 6);
        assert!(
            warned(&fixture.hook_stop_notices).is_empty(),
            "the reset streak does not cap: {:?}",
            warned(&fixture.hook_stop_notices)
        );
    }

    /// Edge case 3: a cancelled turn in the middle of a streak resets the count, at the same
    /// place the validator's count resets. Keeping the count through the cancel caps the
    /// rebuilt streak early.
    #[tokio::test]
    async fn a_cancelled_turn_resets_the_idle_streak() {
        let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
        assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
        assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
        assert_eq!(fixture.hooks.hook_idle_continuations(), 1);
        fixture.hooks.on_turn_end(
            &cx(),
            &completed_outcome(RunStatus::Cancelled(CancelReason::Requested), None),
        );
        assert_eq!(
            fixture.hooks.hook_idle_continuations(),
            0,
            "a non-completed turn resets the streak"
        );
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "the next completed continuation opens a turn"
        );
        assert_eq!(
            fixture.hooks.hook_idle_continuations(),
            0,
            "the first plain turn end starts the rebuilt streak from zero"
        );
        assert!(hook_round(&fixture, &completed_calls(None, 1)).await);
        assert_eq!(fixture.hooks.hook_idle_continuations(), 1);
    }

    /// Edge case 4: a user message while the cap is degraded clears it, and the next hook
    /// exit 2 opens a turn again. Keeping the streak after `reset_wakes` injects instead
    /// and fails the second turn assertion.
    #[tokio::test]
    async fn a_user_message_clears_the_idle_cap() {
        let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
        for round in 0..7 {
            assert!(
                hook_round(&fixture, &completed_calls(None, 1)).await,
                "{round}"
            );
        }
        fixture.hooks.run_stop_once(&completed_calls(None, 1)).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the cap is degraded"
        );
        assert_eq!(warned(&fixture.hook_stop_notices).len(), 1);
        // The same reset the user-message lanes call.
        fixture.hooks.reset_wakes();
        assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
        assert_eq!(fixture.hooks.degraded(), 0);
        assert!(
            hook_round(&fixture, &completed_calls(None, 1)).await,
            "after a user message the next hook exit 2 opens a turn"
        );
        assert_eq!(
            warned(&fixture.hook_stop_notices).len(),
            1,
            "the user message fires no new notice"
        );
    }

    /// Edge case 5: a merged validator and hook continuation counts once in each counter:
    /// the continued turn's end advances the hook idle counter once and the validator's
    /// streak once, and still opens a single turn. Incrementing either twice fails.
    #[tokio::test]
    async fn a_merged_continuation_counts_once_in_each_counter() {
        let fixture = plain(&stop_config("echo hook-reason >&2; exit 2"));
        let hit = "Now the docs-only ADR updates.";
        fixture.hooks.run_stop_once(&completed_text(hit)).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "the merged continuation opens one turn"
        );
        assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
        assert_eq!(fixture.hooks.consecutive_hits(), 1);
        fixture
            .hooks
            .run_stop_once(&completed_calls(Some(hit), 1))
            .await;
        assert_eq!(
            fixture.hooks.hook_idle_continuations(),
            1,
            "the hook counter advances once"
        );
        assert_eq!(
            fixture.hooks.consecutive_hits(),
            2,
            "the validator counter advances once"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "one merged continuation, one turn"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "not two turns for the two counters"
        );
    }

    /// Edge case 6: the 10-per-10-minute window still caps first when every continued turn
    /// calls a tool (each resets the idle counter). The 11th wake degrades through the wake
    /// budget, not the idle cap. Counting tool turns as idle would fire the hook-stop
    /// notice instead.
    #[tokio::test]
    async fn the_wake_window_cap_triggers_first_when_continued_turns_call_tools() {
        tokio::time::pause();
        let fixture = plain(&stop_config("echo keep-going >&2; exit 2"));
        for round in 0..MAX_WAKES_PER_WINDOW {
            // The first turn end is plain; every later one is a continued turn that called
            // a tool (model_calls > 1), which resets the idle streak each time.
            let outcome = completed_calls(None, if round == 0 { 0 } else { 2 });
            assert!(
                hook_round(&fixture, &outcome).await,
                "wake {round} still opens a turn"
            );
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        fixture.hooks.run_stop_once(&completed_calls(None, 2)).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the window gates the 11th continuation"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the gated continuation injects");
        assert!(step.message.text().contains("keep-going"));
        assert_eq!(fixture.hooks.degraded(), 1);
        assert_eq!(
            warned(&fixture.wake_notices).len(),
            1,
            "the wake notice fires, not the idle cap: {:?}",
            warned(&fixture.hook_stop_notices)
        );
        assert!(
            warned(&fixture.hook_stop_notices).is_empty(),
            "tool turns never fill the idle cap"
        );
        assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
    }

    // ---------- `PermissionRequest` (ADR 0045 §4) ----------

    fn permission_request_config(matcher: &str, command: &str) -> HooksConfig {
        HooksConfig {
            permission_request: vec![entry(matcher, vec![hook(command)])],
            ..HooksConfig::default()
        }
    }

    async fn request(
        fixture: &Fixture,
        call: &ToolCall,
    ) -> Result<PermissionRequestDecision, HookError> {
        PermissionRequestHooks::permission_request(&fixture.hooks, call).await
    }

    /// §3: exit 0 with `decision.behavior` answers the ask; a deny carries its `message`.
    #[tokio::test]
    async fn permission_request_allow_and_deny_come_from_the_decision() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#;
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow {
                session_grant: false
            }
        );

        let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no-writes"}}}'"#;
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Deny {
                reason: Some("no-writes".into())
            }
        );
    }

    /// §2: the stdin carries the tool fields and `cwd`/`session_id`, and never
    /// `permission_suggestions` (radian has no text rule syntax).
    #[tokio::test]
    async fn permission_request_stdin_has_the_tool_fields_and_no_suggestions() {
        let root = TempRoot::new("permission-stdin");
        let stdin_path = root.dir.join("stdin.json");
        let config = permission_request_config("shell", &format!("cat > {}", stdin_path.display()));
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        let _ = request(
            &fixture,
            &call("shell", serde_json::json!({"command": "make check"})),
        )
        .await;
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        assert_eq!(stdin["hook_event_name"], "PermissionRequest");
        assert_eq!(stdin["tool_name"], "shell");
        assert_eq!(stdin["tool_input"]["command"], "make check");
        assert_eq!(stdin["session_id"], "test-session");
        assert_eq!(stdin["cwd"], fixture._root.ws.to_string_lossy().as_ref());
        assert!(
            stdin.get("permission_suggestions").is_none(),
            "there are no text suggestions: {stdin}"
        );
    }

    /// §3: exit 2 denies with stderr as the reason (the `PreToolUse` caliber).
    #[tokio::test]
    async fn permission_request_exit_2_denies_with_the_stderr_reason() {
        let fixture = plain(&permission_request_config(
            "shell",
            "echo blocked-by-script >&2; exit 2",
        ));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Deny {
                reason: Some("blocked-by-script\n".into())
            }
        );
    }

    /// §3: empty stdout, an unknown `behavior`, and a decision without `behavior` all mean
    /// "no decision"; the last two warn one line and the ordinary path follows.
    #[tokio::test]
    async fn permission_request_undecided_outputs_follow_the_ordinary_path() {
        let fixture = plain(&permission_request_config("shell", "exit 0"));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::NoDecision
        );
        assert!(warned(&fixture.warnings).is_empty());

        let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"maybe"}}}'"#;
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::NoDecision
        );
        assert!(
            warned(&fixture.warnings)
                .iter()
                .any(|warning| warning.contains("unknown decision.behavior")
                    && warning.contains("maybe")),
            "{:?}",
            warned(&fixture.warnings)
        );

        let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#;
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::NoDecision,
            "items count only from a layer that decides allow"
        );
        assert!(
            warned(&fixture.warnings)
                .iter()
                .any(|warning| warning.contains("without behavior")),
            "{:?}",
            warned(&fixture.warnings)
        );

        let script = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"message":"x"}}}'"#;
        let fixture = plain(&permission_request_config("shell", script));
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::NoDecision
        );
        assert!(
            warned(&fixture.warnings)
                .iter()
                .any(|warning| warning.contains("without behavior")),
            "{:?}",
            warned(&fixture.warnings)
        );
    }

    /// §3 / edge case 3: a non-zero non-2 exit, exit-0 stdout that is not JSON, and a
    /// timeout are hook errors — denied with `hook_error`, never "no decision".
    #[tokio::test]
    async fn permission_request_hook_errors_fail_closed() {
        for (command, timeout) in [
            ("exit 1", None),
            ("printf '%s' 'hello'", None),
            ("sleep 30", Some(1)),
        ] {
            let mut config = permission_request_config("shell", command);
            config.permission_request[0].hooks[0].timeout_secs = timeout;
            let fixture = plain(&config);
            let error = request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .expect_err("a broken hook must fail closed, not decide");
            assert!(
                error.to_string().contains("PermissionRequest hook"),
                "{command}: {error}"
            );
        }
    }

    /// §3: `updatedInput` and `interrupt` are ignored with one warning line, and the
    /// decision still stands.
    #[tokio::test]
    async fn permission_request_ignores_unsupported_decision_fields() {
        for field in [r#""updatedInput":{"command":"x"}"#, r#""interrupt":true"#] {
            let script = format!(
                r#"printf '%s' '{{"hookSpecificOutput":{{"decision":{{"behavior":"allow",{field}}}}}}}'"#
            );
            let fixture = plain(&permission_request_config("shell", &script));
            assert_eq!(
                request(&fixture, &call("shell", serde_json::json!({})))
                    .await
                    .unwrap(),
                PermissionRequestDecision::Allow {
                    session_grant: false
                }
            );
            let warnings = warned(&fixture.warnings);
            assert_eq!(warnings.len(), 1, "{field}: {warnings:?}");
            assert!(
                warnings[0].contains("updatedInput") && warnings[0].contains("interrupt"),
                "{field}: {warnings:?}"
            );
        }
    }

    /// §3: the one accepted `updatedPermissions` item asks for a session grant; other
    /// `type`/`destination`/`behavior` values warn one line and record nothing, and the
    /// allow still stands. `rules` inside an accepted item are not parsed.
    #[tokio::test]
    async fn permission_request_accepts_only_the_session_add_rules_item() {
        let accepted = r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":["Bash(git push)"]}]}}}'"#;
        for (script, session_grant, warnings) in [
            // The one accepted shape: a session grant, whatever `rules` holds.
            (accepted, true, 0usize),
            // Another destination: a warning, and the allow is still one call only.
            (
                r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"projectSettings","behavior":"allow"}]}}}'"#,
                false,
                1,
            ),
            // Another type or behavior: a warning and no grant.
            (
                r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"setMode","destination":"session","behavior":"allow"}]}}}'"#,
                false,
                1,
            ),
            (
                r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"deny"}]}}}'"#,
                false,
                1,
            ),
        ] {
            let fixture = plain(&permission_request_config("shell", script));
            assert_eq!(
                request(&fixture, &call("shell", serde_json::json!({})))
                    .await
                    .unwrap(),
                PermissionRequestDecision::Allow { session_grant },
                "{script}"
            );
            let warned = warned(&fixture.warnings);
            assert_eq!(warned.len(), warnings, "{script}: {warned:?}");
            if warnings > 0 {
                assert!(
                    warned[0].contains("updatedPermissions"),
                    "{script}: {warned:?}"
                );
            }
        }
    }

    /// §3 / rework 2: a present `updatedPermissions` that is not an array warns one line and
    /// records nothing, and the allow still stands. A JSON `null` is present, not an absent
    /// field: deserializing it into a plain `Option<Value>` conflates the two and loses the
    /// warning.
    #[tokio::test]
    async fn a_non_array_updated_permissions_warns_including_null() {
        for value in ["null", r#""session""#, "{}", "42"] {
            let script = format!(
                r#"printf '%s' '{{"hookSpecificOutput":{{"decision":{{"behavior":"allow","updatedPermissions":{value}}}}}}}'"#
            );
            let fixture = plain(&permission_request_config("shell", &script));
            assert_eq!(
                request(&fixture, &call("shell", serde_json::json!({})))
                    .await
                    .unwrap(),
                PermissionRequestDecision::Allow {
                    session_grant: false
                },
                "{value}"
            );
            let warned = warned(&fixture.warnings);
            assert_eq!(warned.len(), 1, "{value}: {warned:?}");
            assert!(
                warned[0].contains("updatedPermissions"),
                "{value}: {warned:?}"
            );
        }
    }

    /// §4: items count only from a layer that decides `allow`: a `deny` layer's
    /// `updatedPermissions` is never read, and the chain's answer stays a denial.
    #[tokio::test]
    async fn a_denying_layer_never_records_an_updated_permissions_grant() {
        let project = permission_request_config(
            "shell",
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#,
        );
        let user = permission_request_config(
            "shell",
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"deny","message":"no-writes","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow"}]}}}'"#,
        );
        let fixture = layered(&user, &project);
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Deny {
                reason: Some("no-writes".into())
            }
        );
    }

    /// §4: the project layer runs first; a user `deny` beats a project `allow`; a project
    /// `deny` stops the chain (the user's marker never appears); and a project `allow`
    /// with a silent user layer allows. An identical command in both layers runs once.
    #[tokio::test]
    async fn permission_request_layers_and_deny_stop_the_chain() {
        // Project allow, user deny: denied.
        let project = permission_request_config(
            "shell",
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#,
        );
        let user = permission_request_config("shell", "echo user-deny >&2; exit 2");
        let fixture = layered(&user, &project);
        match request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap()
        {
            PermissionRequestDecision::Deny { reason } => assert!(
                reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("user-deny")),
                "the user deny stands: {reason:?}"
            ),
            other => panic!("the user deny stands: {other:?}"),
        }

        // Project deny: the user hook never starts.
        let root = TempRoot::new("permission-deny-stops");
        let marker = root.dir.join("user-ran");
        let project = permission_request_config("shell", "echo project-deny >&2; exit 2");
        let user =
            permission_request_config("shell", &format!("touch {}; exit 0", marker.display()));
        let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
        match request(&fixture, &call("shell", serde_json::json!({})))
            .await
            .unwrap()
        {
            PermissionRequestDecision::Deny { reason } => assert!(
                reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("project-deny")),
                "the project deny stands: {reason:?}"
            ),
            other => panic!("the project deny stands: {other:?}"),
        }
        assert!(
            !marker.exists(),
            "a deny only narrows: the later layer must not run"
        );

        // Project allow, user silent: allowed.
        let project = permission_request_config(
            "shell",
            r#"printf '%s' '{"hookSpecificOutput":{"decision":{"behavior":"allow"}}}'"#,
        );
        let user = permission_request_config("shell", "exit 0");
        let fixture = layered(&user, &project);
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow {
                session_grant: false
            }
        );

        // The same command in both layers runs once.
        let root = TempRoot::new("permission-dedupe");
        let counter = root.dir.join("runs");
        let script = format!(
            "echo ran >> {}; printf '%s' '{{\"hookSpecificOutput\":{{\"decision\":{{\"behavior\":\"allow\"}}}}}}'",
            counter.display()
        );
        let project = permission_request_config("shell", &script);
        let user = permission_request_config("shell", &script);
        let fixture = layered_with_root(root, &user, &project, SandboxSlot::empty());
        assert_eq!(
            request(&fixture, &call("shell", serde_json::json!({})))
                .await
                .unwrap(),
            PermissionRequestDecision::Allow {
                session_grant: false
            }
        );
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            1,
            "one run for two identical namings"
        );
    }

    // ---------- async hooks (ADR 0045 §5) ----------

    fn async_hook(command: &str) -> HookCommand {
        HookCommand {
            command: command.to_owned(),
            args: None,
            timeout_secs: None,
            async_hook: true,
            async_rewake: false,
        }
    }

    fn async_rewake_hook(command: &str) -> HookCommand {
        HookCommand {
            command: command.to_owned(),
            args: None,
            timeout_secs: None,
            async_hook: true,
            async_rewake: true,
        }
    }

    fn post_hooks(hooks: Vec<HookCommand>) -> HooksConfig {
        HooksConfig {
            post_tool_use: vec![entry("patch", hooks)],
            ..HooksConfig::default()
        }
    }

    /// Waits until every spawned async hook has finished delivering its result.
    async fn drain_async(fixture: &Fixture) {
        let signal = fixture.hooks.async_signal();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while fixture.hooks.async_hooks_running() > 0 {
            let notified = signal.notified();
            if fixture.hooks.async_hooks_running() == 0 {
                return;
            }
            tokio::time::timeout_at(deadline, notified)
                .await
                .expect("the async hook never finished");
        }
    }

    async fn after_patch(fixture: &Fixture, path: &str) -> ToolResult {
        let mut result = ToolResult::success("c1", vec![ContentPart::text("patched")]);
        fixture
            .hooks
            .after_tool_call(
                &cx(),
                &call("patch", serde_json::json!({"path": path})),
                &mut result,
            )
            .await
            .unwrap();
        result
    }

    /// Edge case 1: an async `PostToolUse` hook that sleeps 2 s does not hold the tool
    /// result, and its `additionalContext` waits in `next_step` for the next model call
    /// (`inject`, never a wake). Running it synchronously fails the timing assertion.
    #[tokio::test]
    async fn an_async_post_hook_returns_at_once_and_injects_before_the_next_model_call() {
        let script =
            r#"sleep 2; printf '%s' '{"hookSpecificOutput":{"additionalContext":"linted"}}'"#;
        let fixture = plain(&post_hooks(vec![async_hook(script)]));
        let started = std::time::Instant::now();
        let result = after_patch(&fixture, "src/a.rs").await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the tool result must not wait for the hook: {:?}",
            started.elapsed()
        );
        assert_eq!(result.text(), "patched", "the result is untouched");
        assert_eq!(
            fixture.hooks.running_async_hooks().len(),
            1,
            "the hook is listed as running"
        );
        drain_async(&fixture).await;
        assert_eq!(fixture.hooks.running_async_hooks().len(), 0);
        let injected = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("additionalContext injects");
        assert_eq!(injected.message.text(), "[PostToolUse hook] linted");
        assert_eq!(injected.source, InputSource::System);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "inject must never wake a turn"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 0);
    }

    /// Edge case 2: three edits to three files in one turn, each matched by its own async
    /// command, produce three separate injected messages (one per result, never merged).
    #[tokio::test]
    async fn three_async_edits_in_one_turn_inject_three_separate_messages() {
        let hook_for = |file: &str, text: &str| {
            async_hook(&format!(
                "input=$(cat); case \"$input\" in *{file}*) printf '%s' \
                 '{{\"hookSpecificOutput\":{{\"additionalContext\":\"{text}\"}}}}';; esac"
            ))
        };
        let fixture = plain(&post_hooks(vec![
            hook_for("a.rs", "a-done"),
            hook_for("b.rs", "b-done"),
            hook_for("c.rs", "c-done"),
        ]));
        for path in ["a.rs", "b.rs", "c.rs"] {
            after_patch(&fixture, path).await;
        }
        drain_async(&fixture).await;
        let mut texts = Vec::new();
        while let Some(entry) = fixture.inbox.claim_next_step().await {
            texts.push(entry.message.text());
        }
        texts.sort();
        assert_eq!(
            texts,
            vec![
                "[PostToolUse hook] a-done",
                "[PostToolUse hook] b-done",
                "[PostToolUse hook] c-done"
            ],
            "one message per async command"
        );
    }

    /// Edge case 3: `asyncRewake` exit 2 wakes an idle session through the `followup`
    /// lane and counts one wake; past the gate it degrades to `inject_degraded` with the
    /// degraded counter and the wake-budget notice (the three visible signals).
    #[tokio::test]
    async fn async_rewake_wakes_once_or_degrades_with_the_three_signals() {
        let fixture = plain(&post_hooks(vec![async_rewake_hook(
            "echo rewake-reason >&2; exit 2",
        )]));
        after_patch(&fixture, "a.rs").await;
        drain_async(&fixture).await;
        let entry = fixture
            .inbox
            .claim_next_turn()
            .await
            .expect("an idle asyncRewake wakes a turn");
        assert_eq!(entry.message.text(), "rewake-reason\n");
        assert_eq!(fixture.hooks.stop_wakes(), 1);
        assert_eq!(fixture.hooks.degraded(), 0);
        assert!(fixture.inbox.claim_next_step().await.is_none());

        let fixture = plain(&post_hooks(vec![async_rewake_hook(
            "echo rewake-reason >&2; exit 2",
        )]));
        for _ in 0..MAX_WAKES_PER_WINDOW {
            fixture.hooks.gate.note_wake();
        }
        after_patch(&fixture, "a.rs").await;
        drain_async(&fixture).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "past the gate no turn opens"
        );
        let step = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("the gated result injects");
        assert_eq!(step.message.text(), "rewake-reason\n");
        assert_eq!(
            fixture.hooks.degraded(),
            1,
            "the degraded signal is the journal-visible count"
        );
        let notices = warned(&fixture.wake_notices);
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("wake budget"), "{notices:?}");
    }

    /// An async exit 2 without `asyncRewake` injects its stderr like `additionalContext`:
    /// it never opens a continuation and never wakes.
    #[tokio::test]
    async fn async_post_exit_2_injects_stderr_without_waking() {
        let fixture = plain(&post_hooks(vec![async_hook(
            "echo lint-failed >&2; exit 2",
        )]));
        let result = after_patch(&fixture, "a.rs").await;
        assert_eq!(result.text(), "patched", "the result is untouched");
        drain_async(&fixture).await;
        let injected = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("exit 2 injects its stderr");
        assert_eq!(injected.message.text(), "lint-failed\n");
        assert_eq!(injected.source, InputSource::System);
        assert!(fixture.inbox.claim_next_turn().await.is_none());
        assert_eq!(fixture.hooks.stop_wakes(), 0);
    }

    /// Edge case 4: an async hook's `permissionDecision` (and the other control fields) is
    /// ignored with one warning; the tool call and its result are unaffected and nothing
    /// is delivered.
    #[tokio::test]
    async fn async_control_fields_are_ignored_with_one_warning() {
        for field in [
            r#""permissionDecision":"deny""#,
            r#""updatedInput":{"path":"elsewhere"}"#,
            r#""updatedToolOutput":"changed""#,
            r#""decision":{"behavior":"deny"}"#,
        ] {
            let script = format!(r#"printf '%s' '{{"hookSpecificOutput":{{{field}}}}}'"#);
            let fixture = plain(&post_hooks(vec![async_hook(&script)]));
            let result = after_patch(&fixture, "a.rs").await;
            assert_eq!(result.text(), "patched", "{field}: the result is untouched");
            assert!(!result.is_error, "{field}");
            drain_async(&fixture).await;
            let warnings = warned(&fixture.warnings);
            assert_eq!(warnings.len(), 1, "{field}: {warnings:?}");
            assert!(
                warnings[0].contains("ignored") && warnings[0].contains("async hook"),
                "{field}: {warnings:?}"
            );
            assert!(fixture.inbox.claim_next_step().await.is_none());
            assert!(fixture.inbox.claim_next_turn().await.is_none());
        }
    }

    /// Edge case 6: a `workspace`-tier async hook runs inside the same sandbox, so a write
    /// outside the workspace fails and lands nothing; the failure is one warning line and
    /// never fails the session closed.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_async_hook_runs_inside_the_workspace_sandbox() {
        let root = TempRoot::new("sandbox-async");
        let outside = root.outside("escaped.txt");
        let config = post_hooks(vec![async_hook(&format!(
            "echo escaped > {}; exit 1",
            outside.display()
        ))]);
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
                .expect("the workspace sandbox probes clean on this machine");
        let fixture = layered_with_root(
            root,
            &config,
            &HooksConfig::default(),
            SandboxSlot::with(sandbox),
        );
        let result = after_patch(&fixture, "a.rs").await;
        assert_eq!(
            result.text(),
            "patched",
            "the denial never changes the result"
        );
        drain_async(&fixture).await;
        assert!(
            !outside.exists(),
            "the write outside the workspace must not land"
        );
        let warnings = warned(&fixture.warnings);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("async hook exited with code"),
            "{warnings:?}"
        );
        let runs = fixture.hooks.hook_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].end, HookRunEnd::Status(1));
    }

    /// An async `Stop` hook is *not* part of the continuation chain when it exits 2: it is
    /// spawned and the turn still ends with no new turn, and the idle-continuation counter
    /// never sees it.
    #[tokio::test]
    async fn an_async_stop_hook_never_joins_the_continuation_chain() {
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![async_hook("echo more >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        fixture.hooks.run_stop_once(&completed_quiet()).await;
        drain_async(&fixture).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "an async Stop hook never opens the turn"
        );
        let injected = fixture
            .inbox
            .claim_next_step()
            .await
            .expect("its exit 2 injects instead");
        assert_eq!(injected.message.text(), "more\n");
        assert_eq!(fixture.hooks.hook_idle_continuations(), 0);
        assert_eq!(fixture.hooks.stop_wakes(), 0);
    }

    /// Edge case 7: an async `Stop` hook is running when the validator decides, so the
    /// validator does not trigger, and the sync hook's `background_tasks` lists the async
    /// one as `type: "hook"`.
    #[tokio::test]
    async fn a_running_async_stop_hook_stands_the_validator_down_and_is_in_background_tasks() {
        let root = TempRoot::new("async-stop-background");
        let stdin_path = root.dir.join("stop-stdin.json");
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![
                    async_hook("sleep 2; exit 0"),
                    hook(&format!("cat > {}", stdin_path.display())),
                ],
            }],
            ..HooksConfig::default()
        };
        let fixture =
            layered_with_root(root, &config, &HooksConfig::default(), SandboxSlot::empty());
        fixture
            .hooks
            .run_stop_once(&completed_text("Now the docs-only ADR updates."))
            .await;
        assert_eq!(
            fixture.hooks.async_hooks_running(),
            1,
            "the async Stop hook is still running"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "a running async hook stands the validator down"
        );
        let stdin: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&stdin_path).unwrap()).unwrap();
        let tasks = stdin["background_tasks"].as_array().expect("an array");
        assert_eq!(tasks.len(), 1, "{stdin}");
        assert_eq!(tasks[0]["type"], "hook");
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["command"], "sleep 2; exit 0");
        drain_async(&fixture).await;
    }

    /// Edge case 8: an async `additionalContext` result never wakes, whether the session
    /// is idle or a turn is in flight when the hook finishes; it waits in `next_step`.
    #[tokio::test]
    async fn async_results_never_wake_idle_or_busy() {
        let script = r#"printf '%s' '{"hookSpecificOutput":{"additionalContext":"done"}}'"#;
        let fixture = plain(&post_hooks(vec![async_hook(script)]));

        // A turn is in flight when the hook finishes (busy).
        fixture
            .inbox
            .followup(Message::user("a turn"), InputSource::User)
            .await
            .unwrap();
        let turn = fixture.inbox.claim_next_turn().await.expect("a turn");
        after_patch(&fixture, "busy.rs").await;
        drain_async(&fixture).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "busy: the result waits for the turn's next model call"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        let injected = fixture.inbox.claim_next_step().await.expect("busy inject");
        assert_eq!(injected.message.text(), "[PostToolUse hook] done");
        fixture.inbox.turn_ended();
        drop(turn);

        // No turn in flight when the hook finishes (idle).
        after_patch(&fixture, "idle.rs").await;
        drain_async(&fixture).await;
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "idle: no wake either"
        );
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert!(fixture.inbox.claim_next_step().await.is_some());
    }
}
