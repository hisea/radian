//! User-level external command hooks (ADR 0045 slice A): the Claude/Codex subset as an
//! [`radian_core::Hooks`] implementation.
//!
//! Three events, all shell commands from the user's own `config.toml` (`[hooks]`):
//!
//! - `PreToolUse` runs before each matching tool call. Exit 0 continues (JSON stdout may
//!   carry `hookSpecificOutput.permissionDecision: "deny"` or `updatedInput`); exit 2
//!   denies the call with the hook's stderr as the reason; any other exit or a timeout is
//!   a hook error and fails closed (ADR 0017: the call is denied with `hook_error`).
//!   `"allow"`/`"ask"` decisions are refused with a warning and treated as no decision —
//!   a hook can only narrow what the permission rules already allowed (ADR 0012 P1).
//!   `updatedInput` becomes [`ToolDecision::Rewrite`]: re-validated, hooks not re-run.
//! - `PostToolUse` runs after each matching call's result, before it is committed. Exit 2
//!   feeds the hook's stderr back to the model alongside the result; any other exit or a
//!   timeout is a hook error and fails closed (the result is replaced with `hook_error`).
//! - `Stop` runs once per completed turn (see [`Hooks::on_turn_end`]: only
//!   [`RunStatus::Completed`] runs the hook; any other terminal status runs nothing).
//!   Exit 2 starts another turn with the hook's stderr as a `System` `followup` through
//!   the inbox, at most a [`WakeGate`] window's worth of continuations; past the gate
//!   it degrades to a marked `inject` (no wake) with the wake-budget notice, the same
//!   degrade as the background-job notifier (ADR 0045 §2).
//!
//! Two layers, one chain (ADR 0045 slice B): the project hooks
//! (`<workspace>/.radian/hooks.json`, loaded only when trusted) run first, then the user's
//! own `[hooks]` — so the user's hooks see the final arguments a project `updatedInput`
//! may have rewritten (Core re-validates a rewrite but never re-runs hooks, ADR 0012 P3).
//! The project layer can never turn a user hook off. Within one event a command string
//! identical to one already run is skipped (one run even when both layers name it); a deny
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
//! the slot they run as plain `/bin/sh -c` with the host environment.

use radian_coding::Jobs;
use radian_core::{
    ContentPart, HookContext, HookError, Hooks, InputSource, Message, RunOutcome, RunStatus,
    ToolCall, ToolDecision, ToolResult,
};
use radian_runtime::Inbox;
use radian_sandbox::SandboxSlot;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use tokio::sync::Notify;

use crate::config::{HookCommand, HookMatcher, HooksConfig};
use crate::goal::SharedGoal;
use crate::intent_stop;
use crate::wake_gate::WakeGate;
use std::sync::Weak;

/// How many characters of a hook's stderr enter the transcript (deny reasons, PostToolUse
/// feedback, Stop continuations). Hooks are the user's own commands, but their output is
/// model context, so it stays bounded.
pub const REASON_CHAR_LIMIT: usize = 2000;

/// How many validator-fired continuations may follow each other: the third consecutive
/// intent-only turn end reports `blocked` instead of opening another turn (backlog
/// `turn-end-continuation.md` §B.3).
pub const MAX_INTENT_STOP_CONSECUTIVE: u32 = 2;

/// The warning channel: the host routes it the way it routes its other startup/runtime
/// lines (the CLI prints it, the TUI shows it as a notice).
pub type HooksWarning = Arc<dyn Fn(&str) + Send + Sync>;

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

/// The user-level command hooks: one [`Hooks`] implementation for all three events.
#[derive(Clone)]
pub struct CommandHooks {
    pre: Vec<CompiledEntry>,
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
    /// Whether the turn now running was opened by a `Stop` continuation (ADR 0045 §2). It
    /// is set when the continuation followup is sent, stays `true` for that turn's own
    /// `Stop` run (`stop_hook_active` on its stdin), and is cleared when that run ends
    /// without opening another, on a non-completed turn, and by any user message
    /// ([`CommandHooks::reset_wakes`]).
    stop_hook_active: Arc<AtomicBool>,
    /// Consecutive validator-fired turn ends. The third consecutive hit reports `blocked`
    /// instead of continuing. A rule miss, a non-completed turn, or a user message
    /// ([`CommandHooks::reset_wakes`]) clears it.
    consecutive: Arc<AtomicU32>,
    warn: HooksWarning,
    /// The wake-budget notice line (routed like `warn`, as its own transcript notice).
    wake: HooksWarning,
    /// The two-in-a-row cap's notice line (its own transcript notice, so the TUI can mark
    /// the session `intent stop` for herdr until the next turn starts).
    intent_stop_warn: HooksWarning,
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
    ) -> Self {
        Self {
            pre: compile_layered(
                &project.pre_tool_use,
                &config.pre_tool_use,
                "PreToolUse",
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
            stop_hook_active: Arc::new(AtomicBool::new(false)),
            consecutive: Arc::new(AtomicU32::new(0)),
            warn,
            wake,
            intent_stop_warn,
        }
    }

    /// Whether no hook at all is configured in either layer.
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.stop.is_empty()
    }

    /// A user message arrived: the `Stop` continuation budget starts over (the same reset
    /// the background-job notifier gets, ADR 0037 decision 3). A degraded continuation
    /// still runs the hook (its exit-2 verdict degrades); only the *wake* is gated. The
    /// validator's consecutive-hit count starts over too, and no continuation is pending
    /// any more: the next `Stop` reports `stop_hook_active: false` (ADR 0045 §2).
    pub fn reset_wakes(&self) {
        self.gate.reset_wakes();
        self.degraded.store(0, Ordering::SeqCst);
        self.consecutive.store(0, Ordering::SeqCst);
        self.stop_hook_active.store(false, Ordering::SeqCst);
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

    /// Whether the turn now running was opened by a `Stop` continuation (tests); its
    /// `Stop` stdin reports this as `stop_hook_active`.
    pub fn stop_hook_active(&self) -> bool {
        self.stop_hook_active.load(Ordering::SeqCst)
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
    /// [`Hooks::on_turn_end`] spawns this with the outcome; tests drive it directly so
    /// there is no timing involved.
    pub async fn run_stop_once(&self, outcome: &RunOutcome) {
        // A non-completed turn ends the continuation: no Stop runs for it and no turn of
        // its own is starting, so the next Stop starts from `false`.
        if !matches!(outcome.status, RunStatus::Completed) {
            self.stop_hook_active.store(false, Ordering::SeqCst);
            return;
        }
        // Claude's field name: the last assistant message's text parts joined, so a user
        // script can run its own detection (backlog §B.6). Empty when the turn has none.
        let last_message = outcome
            .final_message
            .as_ref()
            .map(Message::text)
            .unwrap_or_default();
        // Read this turn's status before running the hooks: the value says whether *this*
        // turn was opened by a continuation. It stays false unless this run opens another
        // one (ADR 0045 §2).
        let stop_hook_active = self.stop_hook_active.swap(false, Ordering::SeqCst);
        let stdin = serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
            "last_assistant_message": last_message,
            "stop_hook_active": stop_hook_active,
            "background_tasks": self.background_tasks(),
            "session_crons": self.session_crons(),
        });
        let mut reasons = Vec::new();
        // One run per command string, like the tool-call events (ADR 0045 slice B).
        let mut seen = std::collections::HashSet::new();
        for entry in &self.stop {
            for hook in &entry.hooks {
                if !seen.insert(hook.command.clone()) {
                    continue;
                }
                match self.execute(hook, &stdin, "Stop").await {
                    Ok(output) if output.code == Some(0) => {}
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
            // ends the degrade burst. Set the flag before the enqueue so a turn that
            // starts immediately already reads `stop_hook_active: true`; a failed
            // enqueue opened nothing and leaves it false.
            self.stop_hook_active.store(true, Ordering::SeqCst);
            if self
                .inbox
                .followup(message, InputSource::System)
                .await
                .is_ok()
            {
                self.gate.note_wake();
                self.degraded.store(0, Ordering::SeqCst);
            } else {
                self.stop_hook_active.store(false, Ordering::SeqCst);
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
            && self.jobs_running() == 0
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

    /// The background jobs running when `Stop` runs, as its `background_tasks` (ADR 0045
    /// §2). A registry that was dropped, or never set, reads as no jobs running: the hook
    /// still runs and the field is `[]`.
    fn background_tasks(&self) -> Vec<serde_json::Value> {
        let jobs = {
            let slot = self
                .jobs
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            slot.as_ref().and_then(Weak::upgrade)
        };
        let Some(jobs) = jobs else {
            return Vec::new();
        };
        jobs.running_tasks()
            .into_iter()
            .map(|task| {
                serde_json::json!({
                    "id": task.id,
                    "type": "shell",
                    "status": "running",
                    "description": task.description,
                    "command": task.command,
                })
            })
            .collect()
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
    /// exit codes are the caller's to interpret.
    ///
    /// The child is a plain `std` process (`SandboxCommand` cannot become a Tokio command):
    /// stdin is written and the output waited on a blocking thread, and a timeout kills the
    /// whole session group (`setsid` in `spawn_piped`) before reaping it.
    async fn execute(
        &self,
        hook: &HookCommand,
        stdin_json: &serde_json::Value,
        event: &str,
    ) -> Result<ProcOutput, HookError> {
        let input = serde_json::to_string(stdin_json)
            .map_err(|error| HookError::new(format!("could not encode the hook input: {error}")))?;
        let mut std_child = self.spawn_piped(hook).map_err(|error| {
            HookError::new(format!("could not start the {event} hook: {error}"))
        })?;
        let pid = std_child.id();
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
            Some(Ok(Ok(output))) => Ok(ProcOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }),
            Some(Ok(Err(error))) => Err(HookError::new(format!(
                "the {event} hook could not finish: {error}"
            ))),
            Some(Err(join)) => Err(HookError::new(format!(
                "the {event} hook task failed: {join}"
            ))),
            None => {
                kill_tree(pid);
                // The kill ends the child, so the waiter returns and reaps it; the
                // result no longer matters.
                let _ = waiter.await;
                Err(HookError::new(format!(
                    "the {event} hook timed out after {timeout_secs}s"
                )))
            }
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
                let mut command = sandbox
                    .command_str("/bin/sh", &["-c", &hook.command])
                    .map_err(std::io::Error::other)?;
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
                let mut command = std::process::Command::new("/bin/sh");
                own_session(&mut command);
                command
                    .arg("-c")
                    .arg(&hook.command)
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
        let output = self.execute(hook, &stdin, "PreToolUse").await?;
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

/// An exit-0 hook stdout, Claude's field names (kept verbatim, hence non-snake-case).
#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
struct HookOutput {
    #[serde(default)]
    hookSpecificOutput: Option<HookSpecificOutput>,
    #[serde(default)]
    reason: Option<String>,
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

/// Bounds hook stderr before it enters the transcript.
fn short(text: &str) -> String {
    let kept: String = text.chars().take(REASON_CHAR_LIMIT).collect();
    if kept.len() < text.len() {
        format!("{kept}…[truncated]")
    } else {
        kept
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
                if !seen.insert(hook.command.clone()) {
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
                if !seen.insert(hook.command.clone()) {
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
                let output = self.execute(hook, &stdin, "PostToolUse").await?;
                match output.code {
                    Some(0) => {}
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
        if !matches!(outcome.status, RunStatus::Completed) {
            // A non-completed turn breaks the validator's chain (like a rule miss), ends
            // any `Stop` continuation it was opened by, and no Stop run will come for
            // this turn: drop the turn-start count.
            self.consecutive.store(0, Ordering::SeqCst);
            self.stop_hook_active.store(false, Ordering::SeqCst);
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
            this.run_stop_once(&outcome).await;
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
            timeout_secs: None,
        }
    }

    fn entry(matcher: &str, commands: Vec<HookCommand>) -> HookMatcher {
        HookMatcher {
            matcher: Some(matcher.to_owned()),
            hooks: commands,
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
        );
        Fixture {
            _root: root,
            hooks,
            warnings,
            wake_notices,
            intent_notices,
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
    /// Never setting the flag fails the middle assertion; never clearing it fails the last.
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
            fixture.hooks.stop_hook_active(),
            "the exit 2 sets the flag for the turn it opens"
        );
        assert!(
            fixture.inbox.claim_next_turn().await.is_some(),
            "the exit 2 opens the continued turn"
        );

        let continued = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            continued["stop_hook_active"],
            serde_json::json!(true),
            "the continued turn says so: {continued}"
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

    /// Edge case 4: a user message while a continuation is pending clears the flag, so the
    /// next `Stop` reports `false`. Dropping the clear from `reset_wakes` (the
    /// followup/steer/inject reset) fails this.
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
        assert!(fixture.hooks.stop_hook_active(), "the exit 2 set the flag");
        assert!(fixture.inbox.claim_next_turn().await.is_some());
        // The same reset the three user-message lanes (followup, steer, inject) call.
        fixture.hooks.reset_wakes();
        let next = stop_stdin_at(&fixture, &stdin_path, &completed_quiet()).await;
        assert_eq!(
            next["stop_hook_active"],
            serde_json::json!(false),
            "a user message ends the continuation: {next}"
        );
    }

    /// Edge case 5: a non-completed (cancelled) turn after a continuation clears the flag;
    /// the next completed turn's `Stop` reports `false`. Keeping the flag through a
    /// non-completed turn fails this.
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
        assert!(fixture.hooks.stop_hook_active(), "the exit 2 set the flag");
        assert!(fixture.inbox.claim_next_turn().await.is_some());
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
}
