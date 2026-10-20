//! User-level external command hooks (ADR 0045 slice A): the Claude/Codex subset as an
//! [`agent_core::Hooks`] implementation.
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

use agent_core::{
    ContentPart, HookContext, HookError, Hooks, InputSource, Message, RunOutcome, RunStatus,
    ToolCall, ToolDecision, ToolResult,
};
use radian_runtime::Inbox;
use radian_sandbox::SandboxSlot;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use tokio::sync::Notify;

use crate::config::{HookCommand, HookMatcher, HooksConfig};
use crate::wake_gate::WakeGate;

/// How many characters of a hook's stderr enter the transcript (deny reasons, PostToolUse
/// feedback, Stop continuations). Hooks are the user's own commands, but their output is
/// model context, so it stays bounded.
pub const REASON_CHAR_LIMIT: usize = 2000;

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
    /// Stop runs currently in flight: `on_turn_end` spawned them and they have not
    /// finished yet. `exec -p` waits for these before deciding the run is over — a
    /// verdict still running may yet open (or degrade) a turn.
    pending: Arc<AtomicU64>,
    /// Bumped whenever a spawned Stop run finishes, so a waiter re-checks.
    pending_signal: Arc<Notify>,
    /// Continuations degraded to inject-only since the last wake. A woken turn absorbs
    /// the waiting injects, so a wake ends the burst; a user message resets it at once.
    degraded: Arc<AtomicU32>,
    warn: HooksWarning,
    /// The wake-budget notice line (routed like `warn`, as its own transcript notice).
    wake: HooksWarning,
}
impl CommandHooks {
    /// Builds the hooks from the resolved `[hooks]` section. Empty when the user configured
    /// nothing, in which case every method is a no-op.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &HooksConfig,
        workspace: &Path,
        session_id: &str,
        sandbox: SandboxSlot,
        inbox: &Arc<Inbox>,
        runtime: &tokio::runtime::Handle,
        warn: HooksWarning,
        wake: HooksWarning,
    ) -> Self {
        Self {
            pre: compile_entries(&config.pre_tool_use, "PreToolUse", &warn),
            post: compile_entries(&config.post_tool_use, "PostToolUse", &warn),
            stop: config.stop.clone(),
            workspace: workspace.to_path_buf(),
            session_id: session_id.to_owned(),
            sandbox,
            inbox: Arc::clone(inbox),
            runtime: runtime.clone(),
            gate: Arc::new(WakeGate::new()),
            degraded: Arc::new(AtomicU32::new(0)),
            pending: Arc::new(AtomicU64::new(0)),
            pending_signal: Arc::new(Notify::new()),
            warn,
            wake,
        }
    }

    /// Whether the user configured no hook at all.
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.stop.is_empty()
    }

    /// A user message arrived: the `Stop` continuation budget starts over (the same reset
    /// the background-job notifier gets, ADR 0037 decision 3). A degraded continuation
    /// still runs the hook (its exit-2 verdict degrades); only the *wake* is gated.
    pub fn reset_wakes(&self) {
        self.gate.reset_wakes();
        self.degraded.store(0, Ordering::SeqCst);
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

    /// Stop runs spawned by `on_turn_end` and not finished yet (tests drive
    /// `run_stop_once` directly, so only spawned runs count).
    pub fn pending_stops(&self) -> u64 {
        self.pending.load(Ordering::SeqCst)
    }

    /// Resolves whenever a spawned Stop run finishes. A waiter re-checks its exit
    /// condition on each one.
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

    /// Runs one `Stop` cycle synchronously: every configured Stop hook in order; the first
    /// exit 2 opens another turn with its stderr as a `System` followup, unless the
    /// continuation cap is already reached — then it degrades to a marked `inject` (no
    /// wake) with the wake-budget notice, so the verdict waits for the next user-woken
    /// turn instead of being dropped silently. Only [`RunStatus::Completed`] runs the hook:
    /// a cancelled, failed, or otherwise non-completed turn runs nothing (an exit-2 hook
    /// after a user interrupt would otherwise open a turn the user just asked to end).
    /// [`Hooks::on_turn_end`] spawns this with the outcome's status; tests drive it
    /// directly so there is no timing involved.
    pub async fn run_stop_once(&self, status: &RunStatus) {
        if !matches!(status, RunStatus::Completed) {
            return;
        }
        if self.stop.is_empty() {
            return;
        }
        let stdin = serde_json::json!({
            "hook_event_name": "Stop",
            "cwd": self.workspace.to_string_lossy(),
            "session_id": self.session_id,
        });
        for entry in &self.stop {
            for hook in &entry.hooks {
                match self.execute(hook, &stdin, "Stop").await {
                    Ok(output) if output.code == Some(0) => {}
                    Ok(output) if output.code == Some(2) => {
                        let reason = short(&output.stderr);
                        let reason = if reason.trim().is_empty() {
                            "the Stop hook exited with code 2".to_owned()
                        } else {
                            reason
                        };
                        let text = format!("Stop hook requested another turn:\n{reason}");
                        // THE WAKE-GATE DEGRADE POINT (ADR 0037 decision 5): past the gate
                        // the hook still runs, but an exit-2 verdict degrades to a marked
                        // `inject` (no wake) with the wake-budget notice, the same degrade
                        // as the background-job notifier (ADR 0045 §2) — a skipped run
                        // would be silent, a degraded verdict waits visibly.
                        if self.gate.wake_allowed() {
                            // The woken turn absorbs the waiting injects, so a wake also
                            // ends the degrade burst.
                            if self
                                .inbox
                                .followup(Message::user(text), InputSource::System)
                                .await
                                .is_ok()
                            {
                                self.gate.note_wake();
                                self.degraded.store(0, Ordering::SeqCst);
                            }
                        } else {
                            if self
                                .inbox
                                .inject_degraded(Message::user(text), InputSource::System)
                                .await
                                .is_ok()
                            {
                                let degraded = self.degraded.fetch_add(1, Ordering::SeqCst) + 1;
                                (self.wake)(&crate::job_notify::wake_budget_line(degraded));
                            }
                        }
                        return;
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

/// Compiles one event's entries once at load time. An invalid pattern warns once here
/// (not per call) and the entry never runs.
fn compile_entries(
    entries: &[HookMatcher],
    event: &str,
    warn: &HooksWarning,
) -> Vec<CompiledEntry> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let matcher = match &entry.matcher {
                None => CompiledMatcher::All,
                Some(pattern) if pattern.trim().is_empty() || pattern.trim() == "*" => {
                    CompiledMatcher::All
                }
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
        })
        .collect()
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
        for entry in &self.pre {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
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
        for entry in &self.post {
            if !Self::matches(&entry.matcher, &call.name) {
                continue;
            }
            for hook in &entry.hooks {
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

    /// A completed turn runs the Stop hooks once, after `RunFinished`; any other terminal
    /// status (cancelled, failed, budget, incomplete, refused, poisoned) runs nothing.
    /// A hook asking for more (exit 2) opens another turn through the inbox. The work
    /// spawns onto the session runtime so this sync callback never blocks a turn.
    fn on_turn_end(&self, _cx: &HookContext, outcome: &RunOutcome) {
        if self.stop.is_empty() {
            return;
        }
        if !matches!(outcome.status, RunStatus::Completed) {
            return;
        }
        // Counted before the spawn, synchronously: the increment happens before the
        // driver's turn-end bookkeeping, so a host that sees the turn end also sees the
        // pending run and waits for its verdict instead of exiting first.
        self.pending.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        let status = outcome.status.clone();
        self.runtime.spawn(async move {
            this.run_stop_once(&status).await;
            this.pending.fetch_sub(1, Ordering::SeqCst);
            this.pending_signal.notify_one();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{CancelReason, CancellationToken};
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
        fixture_with_root(TempRoot::new("case"), config, sandbox)
    }

    fn fixture_with_root(root: TempRoot, config: &HooksConfig, sandbox: SandboxSlot) -> Fixture {
        let (warn, warnings) = warnings_collector();
        let (wake, wake_notices) = warnings_collector();
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            config,
            &root.ws,
            "test-session",
            sandbox,
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
        );
        Fixture {
            _root: root,
            hooks,
            warnings,
            wake_notices,
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
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            &config,
            &root.ws,
            "sid-9",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
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
    /// (fail closed), and nothing lands outside.
    #[tokio::test]
    async fn workspace_runs_the_hook_sandboxed() {
        let root = TempRoot::new("sandbox");
        let outside = root.outside("escaped.txt");
        let config = pre_config("shell", &format!("echo escaped > {}", outside.display()));
        let sandbox =
            radian_sandbox::Sandbox::new(radian_sandbox::SandboxMode::WorkspaceWrite, &root.ws)
                .expect("the workspace sandbox probes clean on this machine");
        let fixture = fixture_with_root(root, &config, SandboxSlot::with(sandbox));
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
        let fixture = fixture_with_root(root, &config, SandboxSlot::with(sandbox));
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
            fixture.hooks.run_stop_once(&RunStatus::Completed).await;
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
            fixture.hooks.run_stop_once(&RunStatus::Completed).await;
            assert!(
                fixture.inbox.claim_next_turn().await.is_some(),
                "continuation {i}: still inside the budget"
            );
            tokio::time::advance(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), MAX_WAKES_PER_WINDOW);
        // The 11th run still runs the hook; its exit-2 verdict degrades to a marked
        // inject (no wake) with the wake-budget notice.
        fixture.hooks.run_stop_once(&RunStatus::Completed).await;
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
        fixture.hooks.run_stop_once(&RunStatus::Completed).await;
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
            fixture.hooks.run_stop_once(&RunStatus::Completed).await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), 3);
        // A user message restarts the budget.
        fixture.hooks.reset_wakes();
        assert_eq!(fixture.hooks.degraded(), 0);
        fixture.hooks.run_stop_once(&RunStatus::Completed).await;
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
            .run_stop_once(&RunStatus::Cancelled(CancelReason::Requested))
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
        fixture.hooks.run_stop_once(&RunStatus::Failed).await;
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
        fixture.hooks.run_stop_once(&RunStatus::Completed).await;
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
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let hooks = CommandHooks::new(
            &config,
            &root.ws,
            "test-session",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
            wake,
        );
        for _ in 0..MAX_WAKES_PER_WINDOW {
            hooks.run_stop_once(&RunStatus::Completed).await;
        }
        hooks.run_stop_once(&RunStatus::Completed).await;
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
}
