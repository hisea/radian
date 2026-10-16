//! The production [`TaskBuilder`] both hosts hand `radian-host` (ADR 0030 slice 6 part b2).
//!
//! Slice 6 part a mounted the lead's tools behind the `/mode` gate but no host passed a task
//! builder, so `spawn_task` could not open a task session outside the tests. This module is the
//! one builder the two hosts share: it builds a **fresh** model for each task through the same
//! `build_model` path the lead used, and a task `SessionSetup` with the host-specific parts the
//! [`crate::task::TaskRegistry`] does not override.
//!
//! What the registry still owns (ADR 0030 O2/O3/O8): the workspace (the worktree), the sandbox
//! network off and `.git` read-only, no memory, the `task` role, the approval routing to the
//! task-side queue, and the end-of-task commit. This builder supplies the rest:
//!
//! - a `workspace` tier and a `workspace_write` profile (the registry adds the task hard-deny
//!   list and the approval policy);
//! - MCP servers **off** by default (ADR 0030 O8: a task mounts no configured server unless one
//!   is passed, and this builder passes none);
//! - the lead's retries, compaction and context-budget settings, skills and wording;
//! - a **silent** event sink and runtime sink: a task's own stream never reaches the lead's live
//!   view. The lead's notices for `TaskSpawned`/`TaskFinished` travel the other way, through the
//!   registry's notifier (see [`crate::task::TaskRegistry::with_notifier`]).
//!
//! The model is built fresh per task from the **same resolved route** as the lead (no credential
//! is copied anywhere new: [`TaskHost::model`] is the host's own `build_model` closure).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{ContextBudget, NoopSink, TurnOptions};
use agent_extensions::McpServerConfig;
use radian_permissions::{ApprovalPolicy, AuditSink, Preset, Profile};
use radian_runtime::{CompactionSettingsFor, ConfigQueue, ConfigSlot, RuntimeEvents};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::SessionJournal;

use crate::agents::SpawnAgent;
use crate::memory::InjectionConfig;
use crate::model::SwitchableModel;
use crate::observer::SessionObserver;
use crate::projection::SessionProjection;
use crate::session::{
    ApprovalRouting, HostBindings, SessionCapability, SessionError, SessionId, SessionSetup,
};
use crate::task::{TaskBuilder, TaskContext, TaskSession, task_sandbox};
use crate::trust;

/// Builds a task's model from the task's session id and a **fresh** config slot. The host
/// implements this with its own `build_model` closure, so a task's model is the lead's route
/// built again — never the lead's model object and never a copied credential. The third
/// argument is the resolved agent definition (ADR 0030 O11): its `model` route and `thinking`
/// level, when present, are the task's (the spawn args already override the definition); the
/// host resolves the route's credential the same way `/model` does.
pub type TaskModelFactory = Arc<
    dyn Fn(&str, Arc<ConfigSlot>, Option<&SpawnAgent>) -> Result<SwitchableModel, String>
        + Send
        + Sync,
>;

/// Everything the builder needs that the hosts resolve anyway for the lead. One value per host
/// process (the two hosts construct it once), cloned per task.
pub struct TaskHost {
    /// Where the task's session file goes: the lead's own session directory (tasks are ordinary
    /// sessions in the same directory, ADR 0030 O2).
    pub session_dir: PathBuf,
    /// The model factory (see [`TaskModelFactory`]).
    pub model: TaskModelFactory,
    /// The provider name and model name, for the task's `SessionCreated` snapshot.
    pub route: (String, String),
    /// The lead's configured MCP servers. A task mounts **none** by default (ADR 0030 O8); the
    /// registry keeps only the servers an agent definition's `mcp` list names, so the builder
    /// hands the configured set and the definition opts in by name.
    pub servers: Vec<McpServerConfig>,
    /// The lead's sandbox, when it has one: the task's deny-read list, Unix-socket whitelist and
    /// environment whitelist are inherited from it (the registry installs a `workspace` policy
    /// with the network off on top).
    pub sandbox: Option<Sandbox>,
    /// The deny-read list the host built for the lead; used only when the lead has no sandbox
    /// (the `safe` tier), so a worktree task is still denied Radian's private directories.
    pub sandbox_deny_read: Vec<PathBuf>,
    /// The lead's Unix-socket whitelist; same purpose as [`TaskHost::sandbox_deny_read`].
    pub allow_unix_sockets: Vec<PathBuf>,
    /// `[model] retries` (ADR 0017), the host's resolved value.
    pub retries: u32,
    /// The context budget the lead started with (ADR 0015).
    pub context_budget: Option<ContextBudget>,
    /// `auto_compact` for the task's turns (ADR 0018).
    pub auto_compact: bool,
    /// The request-level compaction settings (ADR 0018).
    pub compaction_settings: serde_json::Value,
    /// The per-route compaction settings resolver (ADR 0018), when the host has one.
    pub compaction_settings_for: Option<CompactionSettingsFor>,
    /// The turn options, minus any absolute deadline (a task's deadline is its own).
    pub turn_options: TurnOptions,
    /// A per-turn timeout, when the host has one.
    pub turn_timeout: Option<Duration>,
    /// The host's wording (ADR 0012 decision 4).
    pub wording: crate::describe::Wording,
    /// Branch names `git_commit` refuses (ADR 0030 R10): the lead's list.
    pub protected_branches: Vec<String>,
    /// Filter drivers `git add` may run (ADR 0030).
    pub allowed_filters: Option<Vec<String>>,
    /// The lead's `[shell] auto_background_secs` (ADR 0037): a task's foreground
    /// shell calls adopt into a job the same way.
    pub shell_auto_background: Option<Duration>,
    /// Skills discovery inputs (ADR 0025).
    pub skills_extra_paths: Vec<PathBuf>,
    pub skills_builtin: bool,
    pub skills_disabled: Vec<String>,
    /// The memory injection configuration (ADR 0021). A task has no memory root; the
    /// config is still carried so nothing about the session setup is host-specific.
    pub memory_injection: InjectionConfig,
    /// The capabilities the task's facade declares (the two hosts grant all).
    pub capabilities: Vec<SessionCapability>,
    /// The audit sink factory (the same one the lead used). A task's permission decisions are
    /// audited in the task's own journal.
    pub audit: Arc<dyn Fn(Arc<SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync>,
    /// The sink for the task's **own** runtime events. It must not be the lead's live sink: a
    /// task's stream (its turns, its tool calls) never appears in the lead's transcript. The
    /// hosts pass a sink that ignores them (`NoopSink`) — the lead's notice lines come from the
    /// registry's notifier instead (ADR 0030 O10).
    pub task_events: Arc<dyn RuntimeEvents>,
}

/// Builds the production [`TaskBuilder`] from the host's resolved values.
pub fn task_builder(host: TaskHost) -> TaskBuilder {
    Arc::new(move |ctx: &TaskContext<'_>| build_task(&host, ctx))
}

/// One task's [`TaskSession`]. The registry enforces the task invariants on it; everything here
/// is what the host knows and the registry does not.
fn build_task(host: &TaskHost, ctx: &TaskContext<'_>) -> Result<TaskSession, SessionError> {
    let workspace = ctx.workspace.to_path_buf();
    let fail = |error: String| SessionError::Message(error);
    let slot = ConfigSlot::new(serde_json::Value::Null);
    let model = (host.model)(ctx.session_id, Arc::clone(&slot), ctx.agent).map_err(fail)?;
    let sandbox = task_sandbox_for(host, &workspace).map_err(fail)?;
    let profile = Profile::workspace_write(&workspace)
        .map_err(|error| fail(error.to_string()))?
        // The registry replaces this with the spawn's approval policy and adds the task
        // hard-deny list; `auto` here is only the pre-enforcement default.
        .with_approval(ApprovalPolicy::Auto);
    // A task's turn must not inherit the lead's absolute deadline; its other options carry over.
    let mut turn_options = host.turn_options.clone();
    turn_options.deadline = None;
    // The snapshot names the route the task actually runs: the definition's (or an override),
    // else the lead's.
    let (provider, model_name) = ctx
        .agent
        .and_then(|agent| agent.model.clone())
        .map(|route| (route.provider, route.model))
        .unwrap_or_else(|| host.route.clone());
    Ok(TaskSession {
        setup: SessionSetup {
            session_id: SessionId::new(ctx.session_id),
            session_dir: host.session_dir.clone(),
            workspace: workspace.clone(),
            coding_root: Some(workspace.clone()),
            model,
            config_slot: slot,
            config_queue: ConfigQueue::new(),
            // A task does not switch models, and it never resumes, so no budget/price resolver.
            budget_for: None,
            price_for: None,
            protected_branches: host.protected_branches.clone(),
            allowed_filters: host.allowed_filters.clone(),
            shell_auto_background: host.shell_auto_background,
            // ADR 0030 O8: a task mounts no MCP server by default. The builder hands the lead's
            // configured set; the registry keeps only the ones the definition names, so the
            // default stays none and an explicit `mcp` list opts in by name.
            servers: host.servers.clone(),
            // The registry sets the `task` role; `--system` (a fixed layer 1) does not apply.
            role: None,
            system_prompt: None,
            context_budget: host.context_budget,
            auto_compact: host.auto_compact,
            compaction_settings: host.compaction_settings.clone(),
            compaction_settings_for: host.compaction_settings_for.clone(),
            retries: host.retries,
            sandbox,
            tier: Preset::WorkspaceWrite,
            profile,
            // A task has no `/access`; the registry pins the tier.
            access_builder: None,
            // The registry sets these to the task values.
            task_agent: false,
            parent: None,
            allowed_branch: None,
            memory_root: None,
            skills_extra_paths: host.skills_extra_paths.clone(),
            skills_builtin: host.skills_builtin,
            skills_disabled: host.skills_disabled.clone(),
            memory_injection: host.memory_injection,
            // A task's worktree is a fresh, untrusted directory (ADR 0031).
            trust: trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: serde_json::json!({"provider": provider, "model": model_name}),
            turn_options,
            turn_timeout: host.turn_timeout,
            wording: host.wording,
            capabilities: host.capabilities.clone(),
        },
        bindings: HostBindings {
            // The registry replaces this with the task-side routing; `Queued` is the harmless
            // placeholder it overwrites.
            approvals: ApprovalRouting::Queued,
            shared_queue: None,
            ask_user: true,
            audit: Arc::clone(&host.audit),
            // A task's own committed events go nowhere the lead can see.
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            // The host's sink for the task's own runtime events; it must not be the lead's.
            runtime_events: Arc::clone(&host.task_events),
            driver_stopped: Arc::new(|_| {}),
            before_driver: None,
            memory_warning: Arc::new(|_| {}),
            memory_notice_line: Arc::new(|_| {}),
            // Depth limit 2 (ADR 0030 O13): a task never mounts the lead tools.
            task_builder: None,
        },
    })
}

/// The sandbox the builder hands the registry. The registry rebuilds it as a `workspace` policy
/// with the network off and `.git` read-only (ADR 0030 item 2, ADR 0038 decision 5),
/// carrying over the lead's deny-read list, Unix-socket whitelist and environment whitelist.
fn task_sandbox_for(
    host: &TaskHost,
    workspace: &std::path::Path,
) -> Result<Option<Sandbox>, String> {
    if let Some(lead) = &host.sandbox {
        return task_sandbox(lead, workspace)
            .map(Some)
            .map_err(|error| error.to_string());
    }
    // No lead sandbox (the `safe` tier): still give a worktree task a sandbox, with the deny
    // list the host built for the lead, so the task cannot read Radian's private directories.
    let mut policy =
        SandboxPolicy::workspace_write(workspace).map_err(|error| error.to_string())?;
    policy.network = false;
    policy.deny_read = host.sandbox_deny_read.clone();
    policy.allow_unix_sockets = host.allow_unix_sockets.clone();
    Sandbox::from_policy(policy)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// One line per live task for the TUI's `/tasks` (ADR 0030 O10/O12): the id, its state, where
/// it runs, its usage and a bounded change summary. The facts come from the task's committed
/// journal through [`crate::observer::JournalObserver`] — the same read-only view
/// `task_status` uses — so `/tasks` and the lead's tool agree. Empty when there are no tasks.
pub fn task_lines(registry: &crate::task::TaskRegistry) -> Vec<String> {
    registry
        .ids()
        .iter()
        .filter_map(|id| registry.get(id))
        .map(|record| task_line(&record))
        .collect()
}

/// The `/tasks` line for one task.
fn task_line(record: &Arc<crate::task::TaskRecord>) -> String {
    let state = if record.running() {
        "running".to_owned()
    } else {
        match record.last_outcome() {
            Some(outcome) => format!("idle ({outcome:?})"),
            None => "idle".to_owned(),
        }
    };
    let name = match &record.label {
        Some(label) => format!("{} ({label})", record.id),
        None => record.id.clone(),
    };
    let where_ = record
        .worktree
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the shared workspace".to_owned());
    let branch = record.branch.as_deref().unwrap_or("-");
    let usage = match crate::observer::JournalObserver::open(record.session.journal_path()) {
        Ok(mut observer) => {
            let _ = observer.poll();
            let read = observer.projection();
            match (read.usage_input(), read.usage_output()) {
                (Some(input), Some(output)) => format!("{input} in / {output} out"),
                _ => "no usage".to_owned(),
            }
        }
        Err(_) => "no journal".to_owned(),
    };
    let diff = record
        .worktree
        .as_ref()
        .and_then(|worktree| {
            record
                .start_commit
                .as_deref()
                .map(|start| (worktree, start))
        })
        .and_then(|(worktree, start)| {
            let output = radian_coding::host_git::host_git_command(worktree)
                .ok()?
                .args(["diff", "--stat", start])
                .output()
                .ok()?;
            output.status.success().then(|| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .last()
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
        })
        .filter(|summary| !summary.is_empty())
        .unwrap_or_else(|| "no changes".to_owned());
    format!(
        "{name}  {state}  {}  {where_}  {branch}  {usage}  {diff}",
        record.isolation.as_str()
    )
}
