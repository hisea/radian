//! `subagent` (ADR 0043 decision 1a): a blocking, one-shot child `radian exec`.
//!
//! One tool, two built-in kinds: `explore` mounts only the four read-only tools
//! (`read list glob grep`) and `general` mounts the child's ordinary headless set. The child is
//! a real `radian exec` process — the **current executable** (ADR 0032: one binary) — with its
//! own journal, its own session id and `SessionCreated.parent { relation = "subagent" }`.
//!
//! What the tool promises, and what it does not:
//!
//! - **Depth 1.** A child never mounts `subagent`: `radian-host`'s `open_session` drops the
//!   launch parts whenever the session's `parent` relation is `"subagent"`, and the child's
//!   tool set is fixed there. This is the mutation the no-subagent-tool test catches.
//! - **The parent's resolved tier, never wider.** Each call reads the parent's live sandbox
//!   slot (so a `/access` switch is obeyed) and passes `--profile`.
//! - **The parent's approval is not inherited into an answer.** `--yes` is passed only while
//!   the parent's permission hooks are `auto`; otherwise the child is headless with no terminal,
//!   so its `ask` is denied (fail closed) and the result names what was denied.
//! - **The parent's live route, never the startup one.** Each call reads the parent's model
//!   route out of the same config slot a `/model` switch lands in (ADR 0026 D5) and passes
//!   `--provider`/`--model`, so a child started after a switch runs the new model.
//! - **The parent's `--header` values.** They are secrets, so they reach the child through its
//!   environment ([`crate::headers::CHILD_HEADERS_ENV`]), never its argv; the child's startup
//!   adds them to its redaction list. Config headers already reach it through the config file.
//! - **Cancellation stops the child's whole session, in two steps.** SIGTERM first
//!   ([`radian_coding::stop_leader`]): the child cancels its own turn — which is what kills the
//!   session of every command its `shell` tool started, and why one signal is not enough, since
//!   the tool `setsid`s each command — and then closes its session, which kills every background
//!   job's session (ADR 0037 decision 6). Only a child that has not exited within
//!   [`CHILD_STOP_GRACE`] gets [`radian_coding::kill_session`], and only while its leader is
//!   unreaped, so a reused pid is never signalled. Dropping the call's future (a turn timeout
//!   that aborts the driver task, say) takes the same two steps through a drop guard, whose wait
//!   is the synchronous one a drop path can make.
//! - **Not promised: survival rules of the OS.** A parent that exits normally drops the child
//!   handle, which kills the leader (Tokio's `kill_on_drop`); macOS has no `PDEATHSIG`, so a
//!   parent killed with `SIGKILL` can leave the child running. Its journal is on disk and stays
//!   readable with `radian attach`.
//!
//! The result is the child's **last assistant text from its journal** (not its stdout, which is
//! every turn's streamed text), cut at [`MAX_CONCLUSION_CHARS`], plus the child's session id and
//! the usage and denials its journal recorded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    Message, ModelUsage, ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure,
    ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_permissions::{ApprovalPolicy, PermissionHooks};
use radian_runtime::{ConfigSlot, ModelRoute};
use radian_sandbox::{SandboxMode, SandboxSlot};
use serde::Deserialize;
use serde_json::json;

/// The most conclusion characters the model sees. Longer text is cut and marked, so the marker
/// itself is the model's signal that there is more in the child's journal.
pub const MAX_CONCLUSION_CHARS: usize = 4000;

/// How long a stopping child gets to run its own cleanup after SIGTERM before its whole session
/// is SIGKILLed (ADR 0043 decision 1a, fix round 1 B). A few seconds: the child's cancellation
/// kills its `shell` commands in milliseconds, and the rest is its session close. Shorter than
/// the core's `DEFAULT_TOOL_CANCEL_GRACE` (5 s), which is the bound on the call's own future —
/// the tool must reach its SIGKILL before the core drops it.
pub const CHILD_STOP_GRACE: Duration = Duration::from_secs(3);

/// The values a host resolves for the child `radian exec` (ADR 0043 1a point 6). The tool uses
/// the session facts `open_session` already holds (workspace, session directory, tier slot,
/// approval hooks) and this for the rest.
#[derive(Debug, Clone, Default)]
pub struct SubagentParts {
    /// The binary to run: `std::env::current_exe()` for a host. `None` makes every call fail
    /// with a spawn failure instead of guessing a path.
    pub executable: Option<PathBuf>,
    /// The provider descriptor id the parent resolved (`--provider`).
    pub provider: String,
    /// The model the parent resolved (`--model`).
    pub model: String,
    /// The effective endpoint override (`--base-url`), only for a provider that takes one.
    pub base_url: Option<String>,
    /// An explicit `--config` the parent read; a default or `RADIAN_CONFIG` config is inherited
    /// through the environment instead.
    pub config: Option<PathBuf>,
    /// The parent's command-line `--header NAME=VALUE` values, unexpanded. They are **secrets**
    /// and argv is visible to every user through `ps`, so the child receives them in its
    /// environment ([`crate::headers::CHILD_HEADERS_ENV`]) and expands `{session_id}` with its
    /// own id, exactly as the config file's headers do. Config headers are not repeated here:
    /// they reach the child through the config file it reads for itself.
    pub headers: Vec<String>,
    /// `--max-tokens` for anthropic.
    pub max_tokens: Option<u32>,
    /// `--thinking` for anthropic.
    pub thinking: Option<String>,
    /// An explicit `--mcp-config` the parent read.
    pub mcp_config: Option<String>,
    /// Whether the parent trusts this workspace (ADR 0031): a trusted parent passes `--trust`.
    pub trust: bool,
}

/// The provider for the `subagent` tool. Mounted once per parent session; never mounted in a
/// subagent child (depth 1).
pub struct SubagentTools {
    parts: Arc<SubagentParts>,
    session_dir: PathBuf,
    parent_id: String,
    workspace: PathBuf,
    coding_root: Option<PathBuf>,
    sandbox: SandboxSlot,
    /// The parent's live model-route slot (ADR 0026 D5): the driver applies a `/model` change
    /// to it at a turn boundary, so a child started after a switch names the new route.
    route_slot: Arc<ConfigSlot>,
    permissions: Arc<PermissionHooks>,
}

impl SubagentTools {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        parts: SubagentParts,
        session_dir: PathBuf,
        parent_id: impl Into<String>,
        workspace: PathBuf,
        coding_root: Option<PathBuf>,
        sandbox: SandboxSlot,
        route_slot: Arc<ConfigSlot>,
        permissions: Arc<PermissionHooks>,
    ) -> Self {
        Self {
            parts: Arc::new(parts),
            session_dir,
            parent_id: parent_id.into(),
            workspace,
            coding_root,
            sandbox,
            route_slot,
            permissions,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SubagentArgs {
    /// `explore` or `general`; absent means `general`.
    #[serde(default)]
    agent: Option<String>,
    task: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Explore,
    General,
}

impl Kind {
    fn parse(raw: Option<&str>) -> Result<Self, ToolFailure> {
        match raw.unwrap_or("general") {
            "" | "general" => Ok(Self::General),
            "explore" => Ok(Self::Explore),
            other => Err(ToolFailure::new(
                "invalid_agent",
                format!("unknown subagent `{other}`: use `explore` or `general`"),
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::General => "general",
        }
    }
}

/// The `subagent` spec. `Shared` (ADR 0043 1a point 3): several calls in one reply are the
/// parallelism, and the core's own batch concurrency is the only limit.
fn spec() -> ToolSpec {
    ToolSpec {
        name: "subagent".into(),
        description: "Run one task in a fresh blocking child session and return its last \
                      assistant text. `explore` (read list glob grep only) is for reading and \
                      searching; `general` (the default) has this session's tools minus \
                      `subagent` itself. The child starts blank — `task` is its first input — \
                      and inherits this session's permission tier, never a wider one. Several \
                      `subagent` calls in one reply run at the same time; a call blocks until \
                      its child finishes, and this turn's cancellation kills the child."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["explore", "general"],
                    "description": "`explore` is read-only; `general` (the default) has this session's tools minus `subagent`."
                },
                "task": {
                    "type": "string",
                    "description": "The child's first input: what to do, and what to report."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }
}

impl ToolProvider for SubagentTools {
    type Session = SubagentSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(SubagentSession {
            parts: Arc::clone(&self.parts),
            session_dir: self.session_dir.clone(),
            parent_id: self.parent_id.clone(),
            workspace: self.workspace.clone(),
            coding_root: self.coding_root.clone(),
            sandbox: self.sandbox.clone(),
            route_slot: Arc::clone(&self.route_slot),
            permissions: Arc::clone(&self.permissions),
            specs: vec![spec()],
        })
    }
}

/// One parent session's `subagent` tool.
pub struct SubagentSession {
    parts: Arc<SubagentParts>,
    session_dir: PathBuf,
    parent_id: String,
    workspace: PathBuf,
    coding_root: Option<PathBuf>,
    /// The parent's live sandbox slot (ADR 0038 slice 2), so a `/access` switch narrows the
    /// next child too.
    sandbox: SandboxSlot,
    /// The parent's live model route (ADR 0026 D5), read at call time for the same reason.
    route_slot: Arc<ConfigSlot>,
    permissions: Arc<PermissionHooks>,
    specs: Vec<ToolSpec>,
}

impl ToolSession for SubagentSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let args: SubagentArgs =
            serde_json::from_value(call.arguments.clone()).map_err(|error| {
                ToolFailure::new(
                    "invalid_arguments",
                    format!("subagent arguments are not valid: {error}"),
                )
            })?;
        let kind = Kind::parse(args.agent.as_deref())?;
        let Some(executable) = &self.parts.executable else {
            return Err(ToolFailure::new(
                "spawn_failed",
                "cannot find this process's own executable, so no child radian can be started",
            ));
        };
        if kind == Kind::Explore && self.coding_root.is_none() {
            return Err(ToolFailure::new(
                "no_workspace",
                "`explore` needs the read tools, and this session has no workspace to root them \
                 at (start it with --workspace)",
            ));
        }

        // A generated id that cannot collide with a live child: the nanos-pid base plus a
        // per-process counter.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let child_id = format!("{}-s{unique}", crate::generate_session_id());

        let mut command = std::process::Command::new(executable);
        command.args(self.child_args(&child_id, kind, &args.task));
        command.current_dir(&self.workspace);
        // The parent's `--header` values are secrets: argv is world-readable, so they ride the
        // child's environment and its startup adds them to its redaction list (ADR 0043 1a).
        if !self.parts.headers.is_empty() {
            command.env(
                crate::headers::CHILD_HEADERS_ENV,
                crate::headers::encode_child_headers(&self.parts.headers),
            );
        }
        // A child is a session leader with no controlling terminal and no inherited stdio:
        // the journal is the result channel, so its stdout/stderr are not read.
        radian_coding::own_session(&mut command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                ToolFailure::new(
                    "spawn_failed",
                    format!("cannot start the subagent child: {error}"),
                )
            })?;
        let pid = child.id();
        // Declared **after** `child`, so it drops first: if the call's future is dropped while
        // the child runs (a turn timeout aborts the driver task, say), `kill_on_drop` alone
        // would SIGKILL only the leader's pid and a process the child started into its own
        // session would survive. The guard takes the same two steps the cancel branch takes
        // (SIGTERM, the grace, then the session kill), all of them while the leader is still
        // unreaped; the child's own drop then reaps it.
        let mut guard = SessionKillGuard { pid };
        let status = tokio::select! {
            biased;
            _ = cx.cancellation.cancelled() => {
                // The parent turn is over. Stop the child the two-step way (ADR 0043 1a fix
                // round 1 B) while its leader is unreaped, so a reused pid is never signalled.
                stop_child(&mut child, pid).await;
                guard.disarm();
                return Err(ToolFailure::new(
                    "cancelled",
                    format!("the turn was cancelled, so the `{}` subagent child was killed", kind.as_str()),
                ));
            }
            status = child.wait() => {
                // The leader is reaped: its pid may be reused, so the guard must not fire.
                guard.disarm();
                status.map_err(|error| {
                    ToolFailure::new("wait_failed", format!("the subagent child failed: {error}"))
                })?
            }
        };

        let journal = self.session_dir.join(format!("{child_id}.jsonl"));
        let report = read_child_report(&journal);
        let text = render_result(kind, &child_id, status.code(), status.success(), &report);
        Ok(ToolOutput::text(text))
    }
}

impl SubagentSession {
    /// The child's argv (everything after the program name): `radian exec …`.
    fn child_args(&self, child_id: &str, kind: Kind, task: &str) -> Vec<String> {
        let mut args: Vec<String> = vec!["exec".into()];
        args.push("--session-id".into());
        args.push(child_id.to_owned());
        args.push("--session-dir".into());
        args.push(self.session_dir.to_string_lossy().into_owned());
        args.push("--parent".into());
        args.push(self.parent_id.clone());
        args.push("--parent-seq".into());
        args.push("0".into());
        if let Some(root) = &self.coding_root {
            args.push("--workspace".into());
            args.push(root.to_string_lossy().into_owned());
        }
        if kind == Kind::Explore {
            args.push("--tools".into());
            args.push("read,list,glob,grep".into());
        }
        let route = self.current_route();
        if !route.model.is_empty() {
            args.push("--model".into());
            args.push(route.model);
        }
        if !route.provider.is_empty() {
            args.push("--provider".into());
            args.push(route.provider);
        }
        if let Some(base_url) = &self.parts.base_url {
            args.push("--base-url".into());
            args.push(base_url.clone());
        }
        if let Some(config) = &self.parts.config {
            args.push("--config".into());
            args.push(config.to_string_lossy().into_owned());
        }
        if let Some(max_tokens) = self.parts.max_tokens {
            args.push("--max-tokens".into());
            args.push(max_tokens.to_string());
        }
        if let Some(thinking) = &self.parts.thinking {
            args.push("--thinking".into());
            args.push(thinking.clone());
        }
        // `explore` mounts exactly the four read-only tools, and MCP specs are never
        // filtered by `--tools`: passing the parent's config would hand the child tools
        // that can write. A `general` child keeps it — it runs the parent's own set.
        if kind != Kind::Explore {
            if let Some(mcp) = &self.parts.mcp_config {
                args.push("--mcp-config".into());
                args.push(mcp.clone());
            }
        }
        args.push("--profile".into());
        args.push(current_tier(&self.sandbox).to_owned());
        if self.parts.trust {
            args.push("--trust".into());
        }
        // Approval: only the parent's `auto` makes the child auto. Otherwise the child is
        // headless with no terminal, so every `ask` is denied (ADR 0043 1a point 7).
        if self.permissions.approval() == ApprovalPolicy::Auto {
            args.push("--yes".into());
        }
        args.push("-p".into());
        args.push(task.to_owned());
        args
    }

    /// The route the child is told to use **right now** (ADR 0043 1a point 2): the config
    /// slot's model change when a `/model` switch landed (ADR 0026 D5), else the startup
    /// route. The slot is the same one the driver writes and the parent's own model reads, so
    /// the two can never drift.
    fn current_route(&self) -> ModelRoute {
        ModelRoute::from_value(&self.route_slot.value()).unwrap_or_else(|| {
            ModelRoute::new(self.parts.provider.clone(), self.parts.model.clone())
        })
    }
}

/// Stops the child the two-step way (ADR 0043 decision 1a, fix round 1 B), in the cancellation
/// branch: [`radian_coding::stop_leader`] (SIGTERM to the child alone), then — only if it has not
/// exited within [`CHILD_STOP_GRACE`] — [`radian_coding::kill_session`] as the SIGKILL fallback.
///
/// SIGTERM is the step that matters: `radian exec` handles it by cancelling its turn, which is
/// what kills the session of every command its `shell` tool started (the tool `setsid`s each
/// command, so a SIGKILL of the child cannot reach them), and then by closing its session, which
/// kills every background job's session. `child.wait()` only returns once the leader is reaped,
/// so the SIGKILL is sent while the session id is still reserved.
async fn stop_child(child: &mut tokio::process::Child, pid: Option<u32>) {
    let Some(pid) = pid else {
        // The leader already exited: nothing to signal and nothing left to wait for.
        return;
    };
    radian_coding::stop_leader(pid);
    if tokio::time::timeout(CHILD_STOP_GRACE, child.wait())
        .await
        .is_ok()
    {
        return;
    }
    radian_coding::kill_session(pid);
    let _ = child.wait().await;
}

/// Kills the child's whole session when the call's future is dropped before the leader was
/// reaped. `kill_on_drop` alone signals only the leader's pid, so a process the child started
/// into its own session (the `shell` tool `setsid`s every command) survives; the cancel branch
/// already proves the kill must name the session, not the pid. Callers disarm it as soon as the
/// leader is reaped, so a reused pid is never signalled.
struct SessionKillGuard {
    pid: Option<u32>,
}

impl SessionKillGuard {
    /// The leader is reaped (or the kill already happened): nothing left to signal.
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for SessionKillGuard {
    /// The synchronous half of [`stop_child`]: the same two steps, but a drop cannot `await`
    /// `child.wait()` — and could not use it here anyway, because the `Child` is dropped right
    /// after this guard and only that hands it to Tokio's reaper. [`radian_coding::wait_for_exit`]
    /// asks the kernel instead (`waitid` with `WNOWAIT`) and leaves the leader unreaped, so
    /// Tokio's own `kill_on_drop` SIGKILL still names a reserved pid.
    fn drop(&mut self) {
        let Some(pid) = self.pid else {
            return;
        };
        radian_coding::stop_leader(pid);
        if radian_coding::wait_for_exit(pid, CHILD_STOP_GRACE) {
            return;
        }
        radian_coding::kill_session(pid);
    }
}

/// The tier the parent is in **right now** (ADR 0043 1a point 5): the sandbox slot is what
/// `/access` swaps, so the child can only ever be as wide as the parent's current tier.
fn current_tier(sandbox: &SandboxSlot) -> &'static str {
    match sandbox.mode() {
        Some(SandboxMode::WorkspaceWrite) => "workspace",
        Some(SandboxMode::FullAccess) => "full",
        // No sandbox is the `safe` tier; `Some(Safe)` never happens (safe builds none).
        _ => "safe",
    }
}

/// What the child's journal told us after it exited.
#[derive(Debug, Default, PartialEq)]
struct ChildReport {
    last_assistant: Option<String>,
    usage: UsageTotals,
    /// `(tool name, reason)` for each denied call, in journal order and deduplicated by name.
    denied: Vec<(String, Option<String>)>,
}

#[derive(Debug, Default, PartialEq)]
struct UsageTotals {
    calls: usize,
    input: Option<u64>,
    output: Option<u64>,
    total: Option<u64>,
}

impl UsageTotals {
    fn add(&mut self, usage: &ModelUsage) {
        self.calls += 1;
        add_to(&mut self.input, usage.input);
        add_to(&mut self.output, usage.output);
        add_to(&mut self.total, usage.total);
    }

    fn line(&self) -> String {
        if self.calls == 0 || (self.input.is_none() && self.output.is_none()) {
            return "usage: not reported by the provider".to_owned();
        }
        let mut parts = Vec::new();
        if let Some(input) = self.input {
            parts.push(format!("{input} in"));
        }
        if let Some(output) = self.output {
            parts.push(format!("{output} out"));
        }
        if let Some(total) = self.total {
            parts.push(format!("{total} total"));
        }
        format!("usage: {}", parts.join(" / "))
    }
}

fn add_to(slot: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *slot = Some(slot.unwrap_or(0) + value);
    }
}

/// Reads the child's journal for the facts the result carries: the last assistant text, the
/// usage records and the denied permission decisions. Raw `serde_json` lines, the same shape
/// `resumed_model_route` reads; a companion `.jobs` file is never mistaken for a journal.
fn read_child_report(path: &Path) -> ChildReport {
    let mut report = ChildReport::default();
    let mut names: HashMap<String, String> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return report;
    };
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value["event"].as_str() {
            Some("MessageAppended") => {
                let Some(message) = value["data"].get("message") else {
                    continue;
                };
                let Ok(message) = serde_json::from_value::<Message>(message.clone()) else {
                    continue;
                };
                if let Message::Assistant { tool_calls, .. } = &message {
                    // A denied call never reaches `ToolExecutionStarted`, so the assistant
                    // message that asked for it is the only place its name is recorded.
                    for call in tool_calls {
                        names.insert(call.id.clone(), call.name.clone());
                    }
                }
                if matches!(message, Message::Assistant { .. }) {
                    let text = message.text();
                    if !text.trim().is_empty() {
                        report.last_assistant = Some(text);
                    }
                }
            }
            Some("ModelUsageRecorded" | "CompactionUsageRecorded") => {
                if let Some(usage) = value["data"].get("usage")
                    && let Ok(usage) = serde_json::from_value::<ModelUsage>(usage.clone())
                {
                    report.usage.add(&usage);
                }
            }
            Some("ToolExecutionStarted") => {
                if let (Some(call_id), Some(name)) = (
                    value["data"]["call_id"].as_str(),
                    value["data"]["name"].as_str(),
                ) {
                    names.insert(call_id.to_owned(), name.to_owned());
                }
            }
            Some("PermissionResolved") if value["data"]["decision"].as_str() == Some("deny") => {
                let call_id = value["data"]["call_id"].as_str().unwrap_or_default();
                let name = names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.to_owned());
                let reason = value["data"]["reason"].as_str().map(str::to_owned);
                if !report.denied.iter().any(|(seen, _)| *seen == name) {
                    report.denied.push((name, reason));
                }
            }
            _ => {}
        }
    }
    report
}

/// The model-visible result: the conclusion (cut and marked), then the child's facts.
fn render_result(
    kind: Kind,
    child_id: &str,
    exit_code: Option<i32>,
    success: bool,
    report: &ChildReport,
) -> String {
    let mut out = String::new();
    match &report.last_assistant {
        Some(text) => out.push_str(&truncate(text)),
        None => out.push_str("(the subagent child wrote no assistant message)"),
    }
    out.push('\n');
    let status = match exit_code {
        Some(code) => format!("exit {code}"),
        None => "killed by a signal".to_owned(),
    };
    out.push_str(&format!(
        "[subagent {} {child_id}; {status}{}; {}]",
        kind.as_str(),
        if success { "" } else { " (failed)" },
        report.usage.line(),
    ));
    if !report.denied.is_empty() {
        let denied: Vec<String> = report
            .denied
            .iter()
            .map(|(name, reason)| match reason {
                Some(reason) => format!("{name} ({reason})"),
                None => name.clone(),
            })
            .collect();
        out.push_str(&format!("\n[denied: {}]", denied.join("; ")));
    }
    out.push_str(&format!("\n[view it with `radian attach {child_id}`]"));
    out
}

/// Cuts `text` at [`MAX_CONCLUSION_CHARS`] characters, appending the visible marker. Characters,
/// not bytes, so a multi-byte conclusion is never cut mid-character.
fn truncate(text: &str) -> String {
    let total = text.chars().count();
    if total <= MAX_CONCLUSION_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(MAX_CONCLUSION_CHARS).collect();
    format!(
        "{head}\n[truncated: {} more chars]",
        total - MAX_CONCLUSION_CHARS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The last assistant message wins, an assistant that only called tools (no text) is
    /// skipped, and usage records are summed field by field.
    #[test]
    fn the_report_is_the_last_assistant_text_and_the_summed_usage() {
        let dir =
            std::env::temp_dir().join(format!("radian-subagent-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.jsonl");
        let lines = [
            json!({"event": "SessionCreated", "data": {}}).to_string(),
            json!({"event": "MessageAppended", "data": {"message": {"assistant": {"content": [{"text": {"text": "first"}}]}}}}).to_string(),
            json!({"event": "MessageAppended", "data": {"message": {"assistant": {"content": [], "tool_calls": [{"id": "c1", "name": "read", "arguments": {}}]}}}}).to_string(),
            json!({"event": "ModelUsageRecorded", "data": {"usage": {"input": 10, "output": 4}}}).to_string(),
            json!({"event": "MessageAppended", "data": {"message": {"assistant": {"content": [{"text": {"text": "the answer"}}]}}}}).to_string(),
            json!({"event": "ModelUsageRecorded", "data": {"usage": {"input": 5, "total": 9}}}).to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let report = read_child_report(&path);
        assert_eq!(report.last_assistant.as_deref(), Some("the answer"));
        assert_eq!(report.usage.calls, 2);
        assert_eq!(report.usage.input, Some(15));
        assert_eq!(report.usage.output, Some(4));
        assert_eq!(report.usage.total, Some(9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The denied entries name the tool, not the call id. A denied call never executes, so
    /// there is no `ToolExecutionStarted` for it: the name comes from the assistant message
    /// that asked for it.
    #[test]
    fn denied_calls_are_named_by_tool() {
        let dir =
            std::env::temp_dir().join(format!("radian-subagent-denied-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.jsonl");
        let lines = [
            json!({"event": "MessageAppended", "data": {"message": {"assistant": {"content": [], "tool_calls": [{"id": "call-1", "name": "shell", "arguments": {}}]}}}}).to_string(),
            json!({"event": "PermissionResolved", "data": {"request_id": "p", "call_id": "call-1", "decision": "deny", "approver": "user", "reason": "no terminal"}}).to_string(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let report = read_child_report(&path);
        assert_eq!(
            report.denied,
            vec![("shell".to_owned(), Some("no terminal".to_owned()))]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The cut is at the character bound and the marker names the rest.
    #[test]
    fn a_long_conclusion_is_cut_and_marked() {
        let text = "x".repeat(MAX_CONCLUSION_CHARS + 25);
        let cut = truncate(&text);
        assert!(cut.starts_with(&"x".repeat(MAX_CONCLUSION_CHARS)));
        assert!(cut.contains("[truncated: 25 more chars]"), "{cut}");
        assert_eq!(truncate("short"), "short");
    }
}
