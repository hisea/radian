//! The task-session lifecycle (ADR 0030 slice 4): a lead opens a second session in-process as
//! a **task agent**, one per git worktree, with the orchestrator's invariants enforced here.
//!
//! A task agent is an ordinary `open_session` (ADR 0030 O2). What makes it a task agent is
//! the setup the [`TaskRegistry`] hands it:
//!
//! - its own `git worktree` at `~/.radian/worktrees/<repo-key>/<id>` on a fresh `radian/<id>`
//!   branch (ADR 0030 O3, moved **out of the workspace** by ADR 0039 decision 4), created
//!   host-side, outside the sandbox, behind a symlink check on every level of the root; or, with
//!   `isolation = "shared"`, the lead's own workspace forced to the `safe` tier (read-only);
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
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use agent_core::Message;
use radian_permissions::{Preset, Profile};
use radian_sandbox::{GitAccess, Sandbox, SandboxPolicy};
use radian_session::{ForkParent, RuntimeEvent, SessionJournal, TurnOutcome};

use crate::git_commit::{Committed, RepoPin, default_allowed_filters};
use crate::home::{ensure_private_dir, radian_home, worktrees_dir_for};
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

/// The task agents' worktree root (ADR 0039 decision 4): `<radian-home>/worktrees/<repo-key>`,
/// plus the radian home the symlink check starts from.
#[derive(Debug, Clone)]
struct WorktreeRoot {
    home: PathBuf,
    root: PathBuf,
}

/// The registry a lead holds: it keys live task agents by id and owns their worktrees
/// (ADR 0030 slice 4).
pub struct TaskRegistry {
    /// The lead's canonical workspace (the repository the worktrees branch from).
    workspace: PathBuf,
    /// The task worktree root, or `None` when the workspace is not a git repository (or there is
    /// no usable `~/.radian`); a `shared`-only lead can still spawn.
    worktree_root: Option<WorktreeRoot>,
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
        // Canonical, so the worktree paths git reports back match (macOS `/var` -> `/private/var`).
        let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);
        let worktree_root = task_worktree_root(&workspace);
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

    /// The directory task worktrees live under (ADR 0039 decision 4), or `None` when this lead
    /// has no git repository to branch from.
    pub fn worktree_root(&self) -> Option<&Path> {
        self.worktree_root.as_ref().map(|root| root.root.as_path())
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
                let root = self.worktree_root.as_ref().ok_or_else(|| {
                    TaskError::new(format!(
                        "cannot spawn task `{id}` in a worktree: {} is not a git repository, or \
                         there is no usable ~/.radian",
                        self.workspace.display()
                    ))
                })?;
                let worktree = root.root.join(&id);
                let lead = self.workspace.clone();
                let home = root.home.clone();
                let branch = format!("radian/{id}");
                let wt = worktree.clone();
                let br = branch.clone();
                tokio::task::spawn_blocking(move || create_worktree(&lead, &home, &wt, &br))
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

/// The task worktree root for `workspace` (ADR 0039 decision 4), or `None` when the workspace
/// is not a git repository or this process has no usable `~/.radian`.
fn task_worktree_root(workspace: &Path) -> Option<WorktreeRoot> {
    let home = radian_home()?;
    let common = git_common_dir(workspace)?;
    let root = worktrees_dir_for(&home).join(repo_key(&common));
    Some(WorktreeRoot { home, root })
}

/// `<repo-key>`: the repository's directory name plus a short hash of its canonical common dir,
/// so two repositories never share a worktree directory (ADR 0039 decision 4).
fn repo_key(common: &Path) -> String {
    let name = common.file_name().and_then(OsStr::to_str).unwrap_or("");
    // A normal repository's common dir is `<repo>/.git`; the repository's own name is one up.
    let name = if name == ".git" {
        common
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .unwrap_or("")
    } else {
        name
    };
    let mut sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        sanitized = "repo".to_owned();
    }
    format!(
        "{sanitized}-{:012x}",
        // The low 48 bits: a short, stable key a directory name can carry.
        fnv1a64(common.as_os_str().as_encoded_bytes()) & 0x0000_ffff_ffff_ffff
    )
}

/// FNV-1a over the canonical common dir: a stable, dependency-free hash for the directory name.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// `git rev-parse --git-common-dir` for `workspace`, canonicalized; `None` when it is not a git
/// repository.
fn git_common_dir(workspace: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .current_dir(workspace)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(&raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    std::fs::canonicalize(absolute).ok()
}

/// `git worktree add -b <branch> <worktree>` from the lead's workspace (host-side, ADR 0030
/// O3). The worktree root is opened one level at a time with `O_NOFOLLOW|O_DIRECTORY`
/// (ADR 0039 decision 4), so a symlink anywhere refuses before git runs.
fn create_worktree(
    lead: &Path,
    home: &Path,
    worktree: &Path,
    branch: &str,
) -> Result<(), TaskError> {
    let root = worktree.parent().ok_or_else(|| {
        TaskError::new(format!(
            "the worktree path {} has no parent directory",
            worktree.display()
        ))
    })?;
    vet_worktree_root(home, root)?;
    refuse_existing_worktree(root, worktree)?;
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

/// Creates — and checks — every level of `root` below `home` with an `openat` of
/// `O_NOFOLLOW|O_DIRECTORY` from a directory fd (ADR 0039 decision 4). A symlink at any level
/// fails with `ELOOP` and refuses the spawn; a missing level is created owner-only (0700).
#[cfg(unix)]
fn vet_worktree_root(home: &Path, root: &Path) -> Result<(), TaskError> {
    use std::ffi::CString;
    use std::os::unix::io::AsFd;

    ensure_private_dir(home)
        .map_err(|error| TaskError::new(format!("cannot create {}: {error}", home.display())))?;
    let relative = root.strip_prefix(home).map_err(|_| {
        TaskError::new(format!(
            "the task worktree root {} is not below {}",
            root.display(),
            home.display()
        ))
    })?;
    let mut dir = open_dir(home)
        .map_err(|error| TaskError::new(format!("cannot open {}: {error}", home.display())))?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(TaskError::new(format!(
                "refusing the task worktree root {}: it is not a plain path",
                root.display()
            )));
        };
        let name = CString::new(name.as_encoded_bytes())
            .map_err(|_| TaskError::new("the worktree root path is not valid UTF-8"))?;
        let next = match open_dir_at(dir.as_fd(), &name) {
            Ok(next) => next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                make_dir_at(dir.as_fd(), &name).map_err(|error| {
                    TaskError::new(format!(
                        "cannot create a level of {}: {error}",
                        root.display()
                    ))
                })?;
                open_dir_at(dir.as_fd(), &name).map_err(|error| root_error(root, &error))?
            }
            Err(error) => return Err(root_error(root, &error)),
        };
        dir = next;
    }
    Ok(())
}

/// The non-Unix fallback: no `openat`, so each level is checked with `symlink_metadata`.
#[cfg(not(unix))]
fn vet_worktree_root(home: &Path, root: &Path) -> Result<(), TaskError> {
    ensure_private_dir(root)
        .map_err(|error| TaskError::new(format!("cannot create {}: {error}", root.display())))?;
    let relative = root.strip_prefix(home).map_err(|_| {
        TaskError::new(format!(
            "the task worktree root {} is not below {}",
            root.display(),
            home.display()
        ))
    })?;
    let mut level = home.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(TaskError::new(format!(
                "refusing the task worktree root {}: it is not a plain path",
                root.display()
            )));
        };
        level.push(name);
        if std::fs::symlink_metadata(&level).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(TaskError::new(format!(
                "refusing the task worktree root {}: {} is a symlink",
                root.display(),
                level.display()
            )));
        }
    }
    Ok(())
}

/// Refuses a `worktree` that already exists (ADR 0039 decision 4): an existing symlink there
/// would let `git worktree add` write through it. Checked from the root's directory fd with
/// `O_NOFOLLOW`, so only a name that does not exist passes.
#[cfg(unix)]
fn refuse_existing_worktree(root: &Path, worktree: &Path) -> Result<(), TaskError> {
    use std::ffi::CString;
    use std::os::unix::io::AsFd;

    let name = worktree.file_name().ok_or_else(|| {
        TaskError::new(format!(
            "the worktree path {} has no name",
            worktree.display()
        ))
    })?;
    let name = CString::new(name.as_encoded_bytes())
        .map_err(|_| TaskError::new("the worktree path is not valid UTF-8"))?;
    let dir = open_dir(root)
        .map_err(|error| TaskError::new(format!("cannot open {}: {error}", root.display())))?;
    match open_dir_at(dir.as_fd(), &name) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(TaskError::new(format!(
            "refusing to create {}: it already exists",
            worktree.display()
        ))),
        // A symlink (`ELOOP` on Linux, `ENOTDIR` on macOS) and a plain file both land here: it
        // exists and is not a directory, so git must not touch it.
        Err(error) => Err(TaskError::new(format!(
            "refusing to create {}: it is a symlink or not a directory ({error})",
            worktree.display()
        ))),
    }
}

#[cfg(not(unix))]
fn refuse_existing_worktree(_root: &Path, worktree: &Path) -> Result<(), TaskError> {
    match std::fs::symlink_metadata(worktree) {
        Err(_) => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => Err(TaskError::new(format!(
            "refusing to create {}: it is a symlink",
            worktree.display()
        ))),
        Ok(_) => Err(TaskError::new(format!(
            "refusing to create {}: it already exists",
            worktree.display()
        ))),
    }
}

/// Opens `path` as a directory, following symlinks. Used only for the radian home, which the
/// host creates and the sandbox cannot write.
#[cfg(unix)]
fn open_dir(path: &Path) -> std::io::Result<std::os::unix::io::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::OwnedFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)?;
    Ok(OwnedFd::from(file))
}

/// `openat` a child directory with `O_NOFOLLOW|O_DIRECTORY`: a symlink at that name fails with
/// `ELOOP` (ADR 0039 decision 4).
#[cfg(unix)]
fn open_dir_at(
    parent: std::os::unix::io::BorrowedFd<'_>,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::unix::io::OwnedFd> {
    use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `mkdirat` a child directory owner-only (0700) (ADR 0013, ADR 0039 decision 4).
#[cfg(unix)]
fn make_dir_at(
    parent: std::os::unix::io::BorrowedFd<'_>,
    name: &std::ffi::CStr,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The error for a level of the worktree root that could not be opened: a symlink (`ELOOP`) or
/// a non-directory (`ENOTDIR`) is a refusal, anything else is an I/O failure.
#[cfg(unix)]
fn root_error(root: &Path, error: &std::io::Error) -> TaskError {
    match error.raw_os_error() {
        // `O_DIRECTORY|O_NOFOLLOW` on a symlink is `ELOOP` on Linux and `ENOTDIR` on macOS;
        // both mean "this level is a symlink (or another non-directory)", which is a refusal.
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => TaskError::new(format!(
            "refusing the task worktree root {}: a component is a symlink or not a directory",
            root.display()
        )),
        _ => TaskError::new(format!(
            "cannot open the task worktree root {}: {error}",
            root.display()
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repo_key_names_the_repository_and_hashes_the_common_dir() {
        let key = repo_key(Path::new("/Users/u/Code/my-repo/.git"));
        assert!(key.starts_with("my-repo-"), "{key}");
        assert_eq!(key.len(), "my-repo-".len() + 12, "{key}");
        assert!(
            key["my-repo-".len()..]
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "{key}"
        );
        // Stable for the same path, different for another.
        assert_eq!(key, repo_key(Path::new("/Users/u/Code/my-repo/.git")));
        assert_ne!(key, repo_key(Path::new("/Users/u/Code/other/.git")));
        // A bare repository uses its own name; odd characters are folded to `-`.
        assert!(repo_key(Path::new("/srv/foo bar.git")).starts_with("foo-bar.git-"));
    }

    #[test]
    fn the_worktree_root_is_under_the_radian_home_and_not_the_workspace() {
        let home = Path::new("/home/u/.radian");
        let root = worktrees_dir_for(home).join(repo_key(Path::new("/work/example/.git")));
        assert!(root.starts_with(home), "{}", root.display());
        assert_eq!(
            root.parent().unwrap(),
            Path::new("/home/u/.radian/worktrees")
        );
        assert!(!root.starts_with("/work/example"));
    }
}
