//! The task-session lifecycle (ADR 0030 slice 4): a lead opens a second session in-process as
//! a **task agent**, one per git worktree, with the orchestrator's invariants enforced here.
//!
//! A task agent is an ordinary `open_session` (ADR 0030 O2). What makes it a task agent is
//! the setup the [`TaskRegistry`] hands it:
//!
//! - its own `git worktree` at `<workspace>/.radian-worktrees/<id>` on a fresh `radian/<id>`
//!   branch (ADR 0030 O3), created host-side, outside the sandbox; or, with `isolation =
//!   "shared"`, the lead's own workspace forced to the `safe` tier (read-only);
//! - a `workspace` sandbox rooted at the worktree with the **network off** (ADR 0030 revision
//!   2 item 2) and `.git` read-only (ADR 0038 decision 5);
//! - `memory_root = None` (ADR 0030 O8: two sessions writing one memory root would fight over
//!   its single-writer lock);
//! - the `task` role prompt (ADR 0036);
//! - `task_agent = true`, so `/mode` is refused and `git_commit` refuses `main`/`master`
//!   (ADR 0038 decision 4);
//! - `allowed_branch = Some("radian/<id>")`, so the only commits a task can make land on its
//!   own branch (ADR 0030 O3);
//! - `SessionCreated.parent.relation = "task"` naming the lead and its `TaskSpawned` seq
//!   (ADR 0030 O2);
//! - a `HostBindings` whose approvals and questions route to the **lead's** pending queue,
//!   tagged with the task's session id as the origin (ADR 0030 O2), so the human sees one
//!   arrival order and the task never appears as its own queue;
//! - an end-of-task commit: when the task's turn ends, the worktree's uncommitted changes are
//!   committed by the host with the fixed message `radian: end of task <id> (<outcome>)`
//!   (ADR 0030 item 1 B). With no changes, no commit is made.
//!
//! The lead's journal gets a non-critical `TaskSpawned` before the task session is opened, so
//! the lead's own file answers "which task agents did I start" (ADR 0030 O5). `TaskFinished`
//! is written by a later slice when the lead collects the result.
//!
//! The host-specific parts (which model, which profile, the wording, the audit sink) are the
//! caller's: it passes a [`TaskBuilder`] and the registry enforces the invariants above on what
//! the builder returns, so the two hosts and the tests cannot accidentally leave one out.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_core::Message;
use radian_permissions::{Preset, Profile};
use radian_sandbox::{GitAccess, Sandbox, SandboxPolicy};
use radian_session::{ForkParent, RuntimeEvent, SessionJournal, TurnOutcome};

use crate::git_commit::{Committed, RepoPin, default_allowed_filters};
use crate::session::{
    ApprovalRouting, HostBindings, LocalSession, SessionError, SessionQueue, SessionSetup,
    open_session,
};
use crate::session_id::validate_session_id;

/// How a spawned task is isolated (ADR 0030 O3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Isolation {
    /// Its own `git worktree` on `radian/<id>`: the default. The task can change files freely
    /// and the lead reads the result in the worktree.
    #[default]
    Worktree,
    /// No worktree: the task's root is the lead's workspace, forced to the `safe` tier
    /// (read-only). No branch, no end-of-task commit. For tasks that only read.
    Shared,
}

impl Isolation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worktree => "worktree",
            Self::Shared => "shared",
        }
    }
}

/// Why a task could not be spawned or removed.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("{0}")]
    Message(String),
}

impl TaskError {
    fn new(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// What the [`TaskBuilder`] is told about the task it is building a session for. The builder
/// must build its `SessionSetup` for `workspace` (the worktree, or the lead's workspace for
/// `shared`); the registry then enforces the task invariants on the result.
pub struct TaskContext<'a> {
    /// The task id (the session id and the `radian/<id>` branch suffix).
    pub id: &'a str,
    /// The task's session id. Today it is the task id.
    pub session_id: &'a str,
    pub isolation: Isolation,
    /// The task's workspace root: the worktree, or the lead's workspace for `shared`.
    pub workspace: &'a Path,
    /// The `radian/<id>` branch, or `None` for `shared`.
    pub branch: Option<&'a str>,
    /// The `SessionCreated.parent` the task records (relation `"task"`).
    pub parent: &'a ForkParent,
}

/// The host-specific parts of one task session. The registry overrides the task-specific
/// fields (workspace, sandbox network, memory, role, queue, the end-of-task commit); the
/// builder supplies the rest (model, profile, tier, servers, events, audit, wording).
pub struct TaskSession {
    pub setup: SessionSetup,
    pub bindings: HostBindings,
}

/// Builds the host-specific parts of a task session. Called once per [`TaskRegistry::spawn`].
pub type TaskBuilder =
    Arc<dyn Fn(&TaskContext<'_>) -> Result<TaskSession, SessionError> + Send + Sync>;

/// One live task agent: its session and the facts the lead needs about it (ADR 0030 O2/O3).
pub struct TaskRecord {
    pub id: String,
    pub session: Arc<LocalSession>,
    pub isolation: Isolation,
    /// The worktree path, or `None` for `shared`.
    pub worktree: Option<PathBuf>,
    /// The `radian/<id>` branch, or `None` for `shared`.
    pub branch: Option<String>,
    pub label: Option<String>,
    /// The seq of the lead's `TaskSpawned` (the task's `parent.seq`).
    pub start_seq: u64,
    /// The sandbox policy the task's tools run under (ADR 0030 O3): a `workspace` policy with
    /// the network off and `.git` read-only, or `None` for a host with no sandbox.
    pub sandbox_policy: Option<SandboxPolicy>,
}

/// What to spawn. The registry fills the rest from its own configuration.
pub struct SpawnRequest {
    /// The task id: `[A-Za-z0-9._-]`, 1-128 chars (it names a worktree directory and a branch).
    pub id: String,
    pub isolation: Isolation,
    pub label: Option<String>,
    /// The first input, sent as a `followup` with `InputSource::Subagent` (ADR 0030 O2: a task
    /// agent's every input is `Subagent`). `None` opens the session without starting a turn.
    pub brief: Option<String>,
}

impl SpawnRequest {
    /// A worktree task with no brief (the caller sends one, or drives it in a test).
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            isolation: Isolation::Worktree,
            label: None,
            brief: None,
        }
    }
}

/// The registry a lead holds: it keys live task agents by id and owns their worktrees
/// (ADR 0030 slice 4).
pub struct TaskRegistry {
    /// The lead's canonical workspace (the repository the worktrees branch from).
    workspace: PathBuf,
    /// `<workspace>/.radian-worktrees`.
    worktree_root: PathBuf,
    /// The lead's session id, recorded as every task's `parent.session_id`.
    lead_session_id: String,
    /// The lead's journal: `TaskSpawned` lands here (ADR 0030 O5).
    lead_journal: Arc<SessionJournal>,
    /// The lead's pending queue: every task's approvals and questions route here, tagged with
    /// the task's session id (ADR 0030 O2).
    lead_queue: Arc<SessionQueue>,
    builder: TaskBuilder,
    tasks: Mutex<BTreeMap<String, Arc<TaskRecord>>>,
}

impl TaskRegistry {
    /// Builds the registry over the lead's workspace and session.
    pub fn new(
        workspace: impl Into<PathBuf>,
        lead_session_id: impl Into<String>,
        lead_journal: Arc<SessionJournal>,
        lead_queue: Arc<SessionQueue>,
        builder: TaskBuilder,
    ) -> Self {
        let workspace = workspace.into();
        // Canonical, so `<workspace>/.radian-worktrees/<id>` matches what git reports back
        // (macOS `/var` -> `/private/var`).
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        let worktree_root = workspace.join(".radian-worktrees");
        Self {
            workspace,
            worktree_root,
            lead_session_id: lead_session_id.into(),
            lead_journal,
            lead_queue,
            builder,
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// The lead's workspace (the repository task worktrees branch from).
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The live task, if one has that id.
    pub fn get(&self, id: &str) -> Option<Arc<TaskRecord>> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    /// The live task ids, in order.
    pub fn ids(&self) -> Vec<String> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Spawns a task agent (ADR 0030 O2/O3).
    ///
    /// Steps, in order: create the worktree, append the lead's `TaskSpawned`, build and open
    /// the task session with the enforced invariants, and (if a brief was given) send it as a
    /// `Subagent` input. On a failure after the worktree exists, the worktree is removed again.
    pub async fn spawn(&self, request: SpawnRequest) -> Result<Arc<TaskRecord>, TaskError> {
        let SpawnRequest {
            id,
            isolation,
            label,
            brief,
        } = request;
        validate_session_id(&id).map_err(|_| {
            TaskError::new(format!(
                "invalid task id {id:?}: [A-Za-z0-9._-], 1-128 chars"
            ))
        })?;
        if self.get(&id).is_some() {
            return Err(TaskError::new(format!("task `{id}` already exists")));
        }

        // 1. The workspace: a fresh worktree on `radian/<id>`, or the lead's own (shared).
        let (workspace, worktree, branch) = match isolation {
            Isolation::Worktree => {
                let worktree = self.worktree_root.join(&id);
                let lead = self.workspace.clone();
                let branch = format!("radian/{id}");
                let wt = worktree.clone();
                let br = branch.clone();
                tokio::task::spawn_blocking(move || create_worktree(&lead, &wt, &br))
                    .await
                    .map_err(|error| TaskError::new(format!("worktree task failed: {error}")))??;
                (worktree.clone(), Some(worktree), Some(branch))
            }
            Isolation::Shared => (self.workspace.clone(), None, None),
        };

        // 2. The lead's `TaskSpawned`, before the task session exists. `parent.seq` is its seq.
        let start_seq = self.lead_journal.next_seq();
        self.lead_journal
            .append_runtime_event(RuntimeEvent::TaskSpawned {
                agent_session_id: id.clone(),
                worktree: worktree
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                label: label.clone(),
            })
            .await
            .map_err(|error| TaskError::new(format!("cannot write TaskSpawned: {error}")))?;
        let parent = ForkParent::task(self.lead_session_id.clone(), start_seq);

        // 3. Build and open the session, enforcing the task invariants on what the builder gave.
        let parts = (self.builder)(&TaskContext {
            id: &id,
            session_id: &id,
            isolation,
            workspace: &workspace,
            branch: branch.as_deref(),
            parent: &parent,
        })
        .map_err(|error| TaskError::new(error.to_string()))?;
        let TaskSession {
            mut setup,
            mut bindings,
        } = parts;
        let protected = setup.protected_branches.clone();
        let filters = setup
            .allowed_filters
            .clone()
            .unwrap_or_else(default_allowed_filters);
        enforce_task_setup(
            &mut setup,
            &workspace,
            isolation,
            branch.clone(),
            parent.clone(),
        )?;

        // Route the task's approvals and questions to the lead's queue, and commit the task's
        // changes when its turn ends.
        bindings.shared_queue = Some(Arc::clone(&self.lead_queue));
        bindings.approvals = ApprovalRouting::Queued;
        let finisher = Arc::new(TaskFinisher {
            id: id.clone(),
            worktree: worktree.clone(),
            branch: branch.clone(),
            protected,
            filters,
            pin: worktree.as_ref().and_then(|path| RepoPin::resolve(path)),
            journal: OnceLock::new(),
            runtime: tokio::runtime::Handle::current(),
            committed: AtomicBool::new(false),
        });
        let inner = Arc::clone(&bindings.runtime_events);
        bindings.runtime_events = Arc::new(TaskRuntimeEvents {
            inner,
            finisher: Arc::clone(&finisher),
        });

        // The sandbox policy the task's tools will run under (kept for the record): read it
        // before `open_session` consumes the setup.
        let sandbox_policy = setup
            .sandbox
            .as_ref()
            .map(|sandbox| sandbox.policy().clone());
        let session = match open_session(setup, bindings).await {
            Ok(session) => Arc::new(session),
            Err(error) => {
                // The worktree was created before the session; do not leave it orphaned.
                if let Some(worktree) = &worktree {
                    let lead = self.workspace.clone();
                    let wt = worktree.clone();
                    let _ = tokio::task::spawn_blocking(move || remove_worktree(&lead, &wt, true))
                        .await;
                }
                return Err(TaskError::new(error.to_string()));
            }
        };
        let _ = finisher.journal.set(Arc::clone(session.journal()));

        // 4. The brief, as a `Subagent` input (ADR 0030 O2).
        if let Some(brief) = brief {
            session
                .inbox()
                .followup(Message::user(brief), agent_core::InputSource::Subagent)
                .await
                .map_err(|error| TaskError::new(format!("cannot send the brief: {error}")))?;
        }

        let record = Arc::new(TaskRecord {
            id: id.clone(),
            session,
            isolation,
            worktree,
            branch,
            label,
            start_seq,
            sandbox_policy,
        });
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Arc::clone(&record));
        Ok(record)
    }

    /// Closes a task and removes its worktree (ADR 0030 O3: `remove_task`).
    ///
    /// The session is stopped and its driver joined first, so nothing writes while the worktree
    /// goes. Then, unless `force`, a dirty worktree is refused (uncommitted changes are the
    /// user's to look at; after a normal end-of-task commit the worktree is clean). `force`
    /// removes it regardless. The record is kept when the removal is refused.
    pub async fn remove(&self, id: &str, force: bool) -> Result<(), TaskError> {
        let record = self
            .get(id)
            .ok_or_else(|| TaskError::new(format!("no task `{id}`")))?;
        record.session.stop();
        let _ = record.session.join().await;
        if let Some(worktree) = &record.worktree {
            if !force && worktree_is_dirty(worktree)? {
                return Err(TaskError::new(format!(
                    "task `{id}` has uncommitted changes in {}; commit them, or pass `force` \
                     to discard them",
                    worktree.display()
                )));
            }
            let lead = self.workspace.clone();
            let wt = worktree.clone();
            tokio::task::spawn_blocking(move || remove_worktree(&lead, &wt, force))
                .await
                .map_err(|error| TaskError::new(format!("worktree task failed: {error}")))??;
        }
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }
}

/// Enforces the task-agent invariants on a builder's setup (ADR 0030 O2/O3/O8, ADR 0036,
/// ADR 0038 decision 4). Split out so a test can pin each rule without a real session.
fn enforce_task_setup(
    setup: &mut SessionSetup,
    workspace: &Path,
    isolation: Isolation,
    branch: Option<String>,
    parent: ForkParent,
) -> Result<(), TaskError> {
    setup.workspace = workspace.to_path_buf();
    setup.coding_root = Some(workspace.to_path_buf());
    setup.parent = Some(parent);
    setup.allowed_branch = branch;
    setup.task_agent = true;
    // ADR 0030 O8: a task agent does not open memory (its single-writer lock).
    setup.memory_root = None;
    // ADR 0036: the task role prompt. A task agent's role comes from its brief, so
    // `--system` (a fixed layer 1) does not apply.
    setup.role = Some("task".to_owned());
    setup.system_prompt = None;
    match isolation {
        Isolation::Shared => {
            // ADR 0030 O3: `shared` has no worktree and is forced `safe` (read-only). No
            // branch, so no end commit either.
            setup.tier = Preset::Safe;
            setup.sandbox = None;
            setup.profile =
                Profile::safe(workspace).map_err(|error| TaskError::new(error.to_string()))?;
            setup.access_builder = None;
        }
        Isolation::Worktree => {
            // ADR 0030 item 2, ADR 0038 decision 5: the task sandbox keeps the
            // network off and `.git` read-only. Rebuild the policy from the builder's so a
            // host that built it with the network on still gets the off switch, keeping the
            // host's own deny-read list, Unix-socket whitelist and environment whitelist.
            if let Some(sandbox) = setup.sandbox.take() {
                let mut policy = task_sandbox_policy(workspace)?;
                let lead = sandbox.policy();
                policy.deny_read = lead.deny_read.clone();
                policy.allow_unix_sockets = lead.allow_unix_sockets.clone();
                policy.allow_env = lead.allow_env.clone();
                setup.sandbox = Some(
                    Sandbox::from_policy(policy)
                        .map_err(|error| TaskError::new(error.to_string()))?,
                );
            }
        }
    }
    Ok(())
}

/// The task sandbox policy for `workspace` (ADR 0030 item 2, ADR 0038 decision 5):
/// a `workspace` policy with the **network off** and `.git` read-only. The single source of
/// this rule; `enforce_task_setup` and [`task_sandbox`] both start here.
pub fn task_sandbox_policy(workspace: &Path) -> Result<SandboxPolicy, TaskError> {
    let mut policy = SandboxPolicy::workspace_write(workspace)
        .map_err(|error| TaskError::new(error.to_string()))?;
    policy.network = false;
    policy.git_access = GitAccess::ReadOnly;
    Ok(policy)
}

/// Builds a task's sandbox from the lead's: [`task_sandbox_policy`] plus the lead's deny-read
/// list, Unix-socket whitelist and environment whitelist, so the task is not a weaker sandbox
/// than the lead's. Public so a test drives the exact policy the registry installs.
pub fn task_sandbox(lead: &Sandbox, workspace: &Path) -> Result<Sandbox, TaskError> {
    let mut policy = task_sandbox_policy(workspace)?;
    let lead = lead.policy();
    policy.deny_read = lead.deny_read.clone();
    policy.allow_unix_sockets = lead.allow_unix_sockets.clone();
    policy.allow_env = lead.allow_env.clone();
    Sandbox::from_policy(policy).map_err(|error| TaskError::new(error.to_string()))
}

/// `git worktree add <worktree> -b <branch>` from the lead's workspace (host-side, ADR 0030
/// O3). The parent directory is created first: `git worktree add` does not create it.
fn create_worktree(lead: &Path, worktree: &Path, branch: &str) -> Result<(), TaskError> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            TaskError::new(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let path = worktree
        .to_str()
        .ok_or_else(|| TaskError::new("the worktree path is not valid UTF-8"))?;
    let output = std::process::Command::new("git")
        .current_dir(lead)
        .args(["worktree", "add", "-b", branch, path])
        .output()
        .map_err(|error| TaskError::new(format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(TaskError::new(format!(
            "git worktree add -b {branch} {path} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// `git worktree remove [--force] <worktree>` from the lead's workspace.
fn remove_worktree(lead: &Path, worktree: &Path, force: bool) -> Result<(), TaskError> {
    let path = worktree
        .to_str()
        .ok_or_else(|| TaskError::new("the worktree path is not valid UTF-8"))?;
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    let output = std::process::Command::new("git")
        .current_dir(lead)
        .args(&args)
        .output()
        .map_err(|error| TaskError::new(format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(TaskError::new(format!(
            "git worktree remove {path} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Whether the worktree has changes `git status --porcelain` reports. Resolves the pin the way
/// `git_commit` does so a swapped repository is seen, then reads the status.
fn worktree_is_dirty(worktree: &Path) -> Result<bool, TaskError> {
    let pin = RepoPin::resolve(worktree).ok_or_else(|| {
        TaskError::new(format!(
            "{} is not a git repository (or is gone)",
            worktree.display()
        ))
    })?;
    crate::git_commit::dirty(worktree, &pin).map_err(|error| TaskError::new(error.message))
}

/// The word for a turn outcome in the end-of-task commit message.
fn outcome_word(outcome: &TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Refused => "refused",
        TurnOutcome::BudgetExceeded => "budget_exceeded",
        TurnOutcome::ContextBudget => "context_budget",
        TurnOutcome::Failed => "failed",
        TurnOutcome::Poisoned => "poisoned",
        TurnOutcome::Incomplete => "incomplete",
        TurnOutcome::Cancelled => "cancelled",
        _ => "unknown",
    }
}

/// The end-of-task commit (ADR 0030 item 1 B): commit the worktree's uncommitted
/// changes on `radian/<id>` with the fixed message, or make no commit when there are none.
/// Public so a test drives the exact code the runtime runs on turn end.
pub fn end_of_task_commit(
    id: &str,
    outcome: &TurnOutcome,
    worktree: &Path,
    pin: &RepoPin,
    allowed_branch: Option<&str>,
    protected: &[String],
    filters: &[String],
) -> Result<Option<Committed>, agent_core::ToolFailure> {
    if !crate::git_commit::dirty(worktree, pin)? {
        return Ok(None);
    }
    let message = format!("radian: end of task {id} ({})", outcome_word(outcome));
    // `refuse_main = true`: a task agent's rule (ADR 0038 decision 4); with `allowed_branch`
    // it is also limited to its own branch.
    crate::git_commit::commit(
        worktree,
        pin,
        &message,
        &[],
        allowed_branch,
        protected,
        filters,
        true,
    )
    .map(Some)
}

/// The per-task end-of-task commit, invoked on a turn boundary. It needs no `LocalSession`
/// handle (that would be a cycle), only the worktree facts and the task's journal.
struct TaskFinisher {
    id: String,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    protected: Vec<String>,
    filters: Vec<String>,
    pin: Option<RepoPin>,
    /// The task's journal, for the `GitCommitted` audit. Set right after `open_session`; the
    /// finisher only runs on a turn event, which is necessarily after that.
    journal: OnceLock<Arc<SessionJournal>>,
    runtime: tokio::runtime::Handle,
    /// Guard against two overlapping end commits from two turn events.
    committed: AtomicBool,
}

impl TaskFinisher {
    /// On a turn boundary (`TurnFinished`/`TurnFailed`) of a worktree task, run the end-of-task
    /// commit off the runtime thread. `shared` tasks and a second overlapping event do nothing.
    fn on_turn_event(&self, event: &RuntimeEvent) {
        let outcome = match event {
            RuntimeEvent::TurnFinished { outcome, .. } => *outcome,
            RuntimeEvent::TurnFailed { .. } => TurnOutcome::Failed,
            _ => return,
        };
        let Some(worktree) = self.worktree.clone() else {
            return; // shared: no worktree, no end commit (ADR 0030 O3)
        };
        let Some(pin) = self.pin.clone() else {
            return;
        };
        if self.committed.swap(true, Ordering::SeqCst) {
            return;
        }
        let id = self.id.clone();
        let branch = self.branch.clone();
        let protected = self.protected.clone();
        let filters = self.filters.clone();
        let journal = self.journal.get().cloned();
        self.runtime.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                end_of_task_commit(
                    &id,
                    &outcome,
                    &worktree,
                    &pin,
                    branch.as_deref(),
                    &protected,
                    &filters,
                )
            })
            .await;
            // Audit (best effort): the commit already happened or failed; a write failure is
            // not the caller's problem, exactly like `git_commit`'s own audit.
            if let (Ok(Ok(Some(committed))), Some(journal)) = (result, journal) {
                let _ = journal
                    .append_runtime_event(RuntimeEvent::GitCommitted {
                        commit: committed.commit,
                        branch: committed.branch,
                        files: committed.files,
                    })
                    .await;
            }
        });
    }
}

/// Wraps the host's `runtime_events` callback with the task finisher, so both see every event.
struct TaskRuntimeEvents {
    inner: Arc<dyn radian_runtime::RuntimeEvents>,
    finisher: Arc<TaskFinisher>,
}

impl radian_runtime::RuntimeEvents for TaskRuntimeEvents {
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        self.inner.on_runtime_event(event);
        self.finisher.on_turn_event(event);
    }
}
