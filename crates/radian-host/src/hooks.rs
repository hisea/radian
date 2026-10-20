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
//! - `Stop` runs once per turn end (see [`Hooks::on_turn_end`]). Exit 2 starts another
//!   turn with the hook's stderr as a `System` `followup` through the inbox, at most
//!   [`STOP_MAX_CONTINUATIONS`] consecutive continuations.
//!
//! The matcher names **Radian** tool names as exact names joined by `|` (a missing or
//! empty matcher matches every tool); `Stop` ignores it. There is no runtime aliasing of
//! Claude's tool names.
//!
//! Hook commands run inside the same OS sandbox the `shell` tool uses when the session has
//! one (enforced or not, with the same environment rules as `shell`); with no sandbox in
//! the slot they run as plain `/bin/sh -c` with the host environment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::{
    ContentPart, HookContext, HookError, Hooks, InputSource, Message, RunOutcome, ToolCall,
    ToolDecision, ToolResult,
};
use radian_runtime::Inbox;
use radian_sandbox::SandboxSlot;
use serde::Deserialize;

use crate::config::{HookCommand, HookMatcher, HooksConfig};

/// How many consecutive `Stop`-hook continuations may open a new turn before the hook is
/// skipped until the next user message (ADR 0045; the same shape as ADR 0037's
/// `MAX_CONSECUTIVE_WAKES`). Fixed, no config key.
pub const STOP_MAX_CONTINUATIONS: u32 = 3;

/// How many characters of a hook's stderr enter the transcript (deny reasons, PostToolUse
/// feedback, Stop continuations). Hooks are the user's own commands, but their output is
/// model context, so it stays bounded.
pub const REASON_CHAR_LIMIT: usize = 2000;

/// The warning channel: the host routes it the way it routes its other startup/runtime
/// lines (the CLI prints it, the TUI shows it as a notice).
pub type HooksWarning = Arc<dyn Fn(&str) + Send + Sync>;

/// The user-level command hooks: one [`Hooks`] implementation for all three events.
#[derive(Clone)]
pub struct CommandHooks {
    pre: Vec<HookMatcher>,
    post: Vec<HookMatcher>,
    stop: Vec<HookMatcher>,
    workspace: PathBuf,
    session_id: String,
    sandbox: SandboxSlot,
    inbox: Arc<Inbox>,
    runtime: tokio::runtime::Handle,
    /// Consecutive `Stop` continuations with no user message in between.
    stop_wakes: Arc<AtomicU32>,
    warn: HooksWarning,
}

impl CommandHooks {
    /// Builds the hooks from the resolved `[hooks]` section. Empty when the user configured
    /// nothing, in which case every method is a no-op.
    pub fn new(
        config: &HooksConfig,
        workspace: &Path,
        session_id: &str,
        sandbox: SandboxSlot,
        inbox: &Arc<Inbox>,
        runtime: &tokio::runtime::Handle,
        warn: HooksWarning,
    ) -> Self {
        Self {
            pre: config.pre_tool_use.clone(),
            post: config.post_tool_use.clone(),
            stop: config.stop.clone(),
            workspace: workspace.to_path_buf(),
            session_id: session_id.to_owned(),
            sandbox,
            inbox: Arc::clone(inbox),
            runtime: runtime.clone(),
            stop_wakes: Arc::new(AtomicU32::new(0)),
            warn,
        }
    }

    /// Whether the user configured no hook at all.
    pub fn is_empty(&self) -> bool {
        self.pre.is_empty() && self.post.is_empty() && self.stop.is_empty()
    }

    /// A user message arrived: the `Stop` continuation budget starts over (the same reset
    /// the background-job notifier gets, ADR 0037 decision 3).
    pub fn reset_wakes(&self) {
        self.stop_wakes.store(0, Ordering::SeqCst);
    }

    /// Consecutive `Stop`-hook continuations so far (tests).
    pub fn stop_wakes(&self) -> u32 {
        self.stop_wakes.load(Ordering::SeqCst)
    }

    /// The matcher: exact Radian tool names joined by `|` (Claude's regex-ish matcher
    /// reduced to what this slice promises). A missing or empty matcher matches everything.
    fn matches(matcher: &Option<String>, tool: &str) -> bool {
        match matcher {
            None => true,
            Some(pattern) if pattern.trim().is_empty() => true,
            Some(pattern) => pattern.split('|').any(|name| name.trim() == tool),
        }
    }

    /// Runs one `Stop` cycle synchronously: every configured Stop hook in order; the first
    /// exit 2 opens another turn with its stderr as a `System` followup, unless the
    /// continuation cap is already reached. [`Hooks::on_turn_end`] spawns this; tests drive
    /// it directly so there is no timing involved.
    pub async fn run_stop_once(&self) {
        if self.stop.is_empty() {
            return;
        }
        if self.stop_wakes.load(Ordering::SeqCst) >= STOP_MAX_CONTINUATIONS {
            // The cap is reached: skip the run entirely until the next user message.
            // Running a validation whose verdict cannot continue the turn would only
            // spend the model wait for nothing.
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
                        if self
                            .inbox
                            .followup(Message::user(text), InputSource::System)
                            .await
                            .is_ok()
                        {
                            self.stop_wakes.fetch_add(1, Ordering::SeqCst);
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
                stdin.write_all(input.as_bytes())?;
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

    /// Every turn end runs the Stop hooks once, after `RunFinished` — normal, failed and
    /// cancelled alike — and a hook asking for more (exit 2) opens another turn through the
    /// inbox. The work spawns onto the session runtime so this sync callback never blocks a
    /// turn.
    fn on_turn_end(&self, _cx: &HookContext, _outcome: &RunOutcome) {
        if self.stop.is_empty() {
            return;
        }
        let this = self.clone();
        self.runtime.spawn(async move {
            this.run_stop_once().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::CancellationToken;
    use std::sync::Mutex;

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
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            config,
            &root.ws,
            "test-session",
            sandbox,
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
        );
        Fixture {
            _root: root,
            hooks,
            warnings,
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

    /// Claude's stdin field names reach the hook: event, tool, input, cwd, session id.
    #[tokio::test]
    async fn the_hook_sees_claudes_stdin_fields() {
        let root = TempRoot::new("stdin");
        let stdin_path = root.dir.join("stdin.json");
        let config = pre_config("shell", &format!("cat > {}", stdin_path.display()));
        let (warn, _) = warnings_collector();
        let inbox = Inbox::new(64);
        let hooks = CommandHooks::new(
            &config,
            &root.ws,
            "sid-9",
            SandboxSlot::empty(),
            &inbox,
            &tokio::runtime::Handle::current(),
            warn,
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

    /// `Stop` exit 2 opens another turn with the stderr as a `System` followup — at most
    /// three consecutive continuations, then the hook is skipped until a user message.
    #[tokio::test]
    async fn stop_exit_2_continues_at_most_three_times() {
        let config = HooksConfig {
            stop: vec![HookMatcher {
                matcher: None,
                hooks: vec![hook("echo keep-going >&2; exit 2")],
            }],
            ..HooksConfig::default()
        };
        let fixture = plain(&config);
        for _ in 0..5 {
            fixture.hooks.run_stop_once().await;
        }
        assert_eq!(fixture.hooks.stop_wakes(), 3);
        // `claim_next_turn` is non-blocking: three followups are there to take.
        for _ in 0..3 {
            let entry = fixture
                .inbox
                .claim_next_turn()
                .await
                .expect("three followups are enqueued");
            assert!(
                entry.message.text().contains("keep-going"),
                "the stderr is the input: {}",
                entry.message.text()
            );
            assert_eq!(entry.source, InputSource::System);
        }
        // The 4th (and 5th) continuation never happens: no followup, no inject either.
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "the 4th continuation must not open a turn"
        );
        assert!(
            fixture.inbox.claim_next_step().await.is_none(),
            "the capped hook must not inject either"
        );
        // A user message restarts the budget.
        fixture.hooks.reset_wakes();
        fixture.hooks.run_stop_once().await;
        assert_eq!(fixture.hooks.stop_wakes(), 1);
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
        fixture.hooks.run_stop_once().await;
        assert_eq!(fixture.hooks.stop_wakes(), 0);
        assert!(
            fixture.inbox.claim_next_turn().await.is_none(),
            "exit 0 continues nothing"
        );
    }
}
