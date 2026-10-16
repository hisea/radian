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
//! - a `HostBindings` whose approvals and questions route to the **task-side** pending queue
//!   ([`crate::task_pending`], ADR 0030 slice 5, revision 6): the task's still-ask approvals and
//!   its `ask_user` become pending task events the lead resolves (answer / deny / escalate),
//!   never items in the human's queue. Only an escalation the lead performs reaches the human
//!   queue, tagged with the task as the origin (ADR 0030 R9);
//! - an end-of-task commit: when the task's turn ends, the worktree's uncommitted changes are
//!   committed by the host with the fixed message `radian: end of task <id> (<outcome>)`
//!   (ADR 0030 item 1 B). With no changes, no commit is made. The commit is
//!   **awaitable** (ADR 0039 decision 7): [`TaskRecord::await_end_commit`] resolves it, and
//!   [`TaskRegistry::remove`] waits for it.
//! - exactly one `TaskFinished` when the task ends (ADR 0030 O5): after the end-of-task commit
//!   for a worktree task (`shared` has no commit, so at the turn end), or `cancelled` once when
//!   the task is removed before any turn ended. A second turn end, and a removal after a normal
//!   end, write nothing: the first end is the task's end (ADR 0030 O3, one task is one turn).
//!
//! Spawning is one transaction (ADR 0039 decision 5). A [`SpawnGuard`] owns the worktree from
//! the moment it exists and — after `open_session` — the session's driver; it is disarmed only
//! once the record is in the registry. A failure after the lead's `TaskSpawned` was written
//! pairs it with a `TaskFinished { outcome: failed, error }` (ADR 0030 O5, ADR 0039 decision 5);
//! a failure before that leaves the journal untouched.
//!
//! The host-specific parts (which model, which profile, the wording, the audit sink) are the
//! caller's: it passes a [`TaskBuilder`] and the registry enforces the invariants above on what
//! the builder returns, so the two hosts and the tests cannot accidentally leave one out. The
//! [`TaskSeams`] the registry can carry exist only for tests.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_core::Message;
use chrono::{DateTime, Utc};
use radian_coding::HeavySlots;
use radian_permissions::{ApprovalPolicy, Preset, Profile};
use radian_sandbox::{GitAccess, Sandbox, SandboxPolicy};
use radian_session::{ForkParent, RuntimeEvent, SessionJournal, TurnOutcome};
use tokio::sync::{Notify, watch};

use crate::agents::{SpawnAgent, SpawnOverrides, resolve as resolve_agent};
use crate::git_commit::{Committed, RepoPin, default_allowed_filters};
use crate::home::{ensure_private_dir, radian_home, worktrees_dir_for};
use crate::observer::{JournalObserver, SessionObserver};
use crate::session::{
    ApprovalRouting, HostBindings, LocalSession, SessionError, SessionQueue, SessionSetup,
    open_session,
};
use crate::session_id::validate_session_id;
use crate::task_pending::{
    TaskApprover, TaskPendingError, TaskPendingQueue, TaskPendingView, TaskPrompter,
};

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

/// The orchestration limits ADR 0030 O8 gives a [`TaskRegistry`]. They are **constructor
/// parameters with the ADR defaults**; reading them from `[orchestration]` is slice 9, not here.
#[derive(Debug, Clone)]
pub struct OrchestrationLimits {
    /// At most this many tasks run at once in one registry (O8: default 4). `spawn_task` past
    /// the cap queues the task, FIFO.
    pub max_tasks: usize,
    /// The heavy-command slots (O8/R4: default 2), process-level: installed with
    /// [`HeavySlots::set_process`] so the lead and every task share one pool.
    pub heavy_slots: usize,
    /// The commands that count as heavy (O8/R4: `cargo build`/`cargo test`/`cargo clippy`).
    pub heavy_patterns: Vec<String>,
    /// A task with no committed event or successful write for this long is `stalled` (O8/R5:
    /// default 600 s). The runtime never intervenes.
    pub stall_after_secs: f64,
}

impl Default for OrchestrationLimits {
    fn default() -> Self {
        Self {
            max_tasks: 4,
            heavy_slots: radian_coding::DEFAULT_HEAVY_SLOTS,
            heavy_patterns: radian_coding::DEFAULT_HEAVY_PATTERNS
                .iter()
                .map(|pattern| (*pattern).to_owned())
                .collect(),
            stall_after_secs: 600.0,
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
    /// The agent definition resolved and snapshotted at spawn (ADR 0030 O11, slice 7). The
    /// builder builds the model from its route and thinking level; the registry enforces the
    /// narrowing rules on the rest. `None` only for a registry that resolves nothing (a direct
    /// unit test of [`enforce_task_setup`]).
    pub agent: Option<&'a SpawnAgent>,
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

/// A callback the registry calls for each `TaskSpawned`/`TaskFinished` it writes to the lead's
/// journal, so the host's live view can show a notice line (ADR 0030 O10). Installed by the two
/// hosts with [`TaskRegistry::with_notifier`]; `None` in tests, so no notice outside a host.
pub type TaskNotifier = Arc<dyn Fn(&RuntimeEvent) + Send + Sync>;

/// Where a spawn can be interrupted, for tests (ADR 0039 decision 5). Production installs no
/// hook, so no variant ever fires outside a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnStage {
    /// Before the lead's `TaskSpawned` is appended: models a write that fails, so nothing is
    /// recorded and nothing needs pairing.
    TaskSpawned,
    /// Before the [`TaskBuilder`] builds the session.
    Builder,
    /// Before the task invariants are enforced on the built setup.
    Enforce,
    /// Before the brief is sent as a `Subagent` input.
    Brief,
}

/// A test seam run at each [`SpawnStage`]; `Err` aborts the spawn there with that message.
pub type SpawnHook = Arc<dyn Fn(SpawnStage) -> Result<(), String> + Send + Sync>;

/// A test seam run at the start of the end-of-task commit, on the blocking thread, so a test
/// can hold the commit and prove [`TaskRegistry::remove`] waits for it (ADR 0039 decision 7).
pub type CommitGate = Arc<dyn Fn() + Send + Sync>;

/// The test seams a registry can carry (ADR 0039 decisions 5 and 7). Both are `None` in
/// production; the two hosts never install one.
#[derive(Clone, Default)]
pub struct TaskSeams {
    pub spawn: Option<SpawnHook>,
    pub commit_gate: Option<CommitGate>,
}

/// How a task's end-of-task commit ended (ADR 0030 item 1 B, ADR 0039 decision 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndCommit {
    /// The worktree was clean; no commit was made.
    None,
    /// The commit landed.
    Committed(Committed),
    /// The commit failed; the message is what a `TaskFinished` carries.
    Failed(String),
}

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
    /// The agent definition name the task was spawned from (ADR 0030 O11); the task default is
    /// `task`.
    pub agent: String,
    /// Whether the definition asked for `spawn: true`, ignored because a task is depth 2
    /// (ADR 0030 O13).
    pub spawn_ignored: bool,
    /// Spawn-time warnings about the agent definition (ADR 0030 O11), today a `skills` name
    /// discovery did not find. Empty when the definition names none. A warning is never a
    /// failure; the lead's `spawn_task` result carries these.
    pub warnings: Vec<String>,
    /// The seq of the lead's `TaskSpawned` (the task's `parent.seq`).
    pub start_seq: u64,
    /// The commit the worktree's branch was created from (the lead's HEAD at spawn), so a later
    /// `git diff --stat` is relative to the start (ADR 0030 R8). `None` for `shared`
    /// and when the worktree is not a repository.
    pub start_commit: Option<String>,
    /// The sandbox policy the task's tools run under (ADR 0030 O3): a `workspace` policy with
    /// the network off and `.git` read-only, or `None` for a host with no sandbox.
    pub sandbox_policy: Option<SandboxPolicy>,
    /// When the task started, the reference for the stall check before it has an event or a
    /// write (ADR 0030 R5).
    pub started_at: DateTime<Utc>,
    /// The awaitable end-of-task commit (ADR 0039 decision 7). Private so only the registry
    /// builds a record.
    finisher: Arc<TaskFinisher>,
}

impl TaskRecord {
    /// Waits for the end-of-task commit to finish (ADR 0039 decision 7). `None` when no turn
    /// has ended, so no commit was ever scheduled.
    pub async fn await_end_commit(&self) -> Option<EndCommit> {
        self.finisher.await_end_commit().await
    }

    /// Whether the task's turn is running right now (ADR 0030 slice 6's `wait_task`): `true`
    /// from the committed `TurnStarted` until the next turn boundary.
    pub fn running(&self) -> bool {
        self.finisher.running()
    }

    /// The outcome of the last turn the task ended, or `None` before its first. `wait_task`/
    /// `wait_any` read it: a task that is not running with a `Some` outcome has finished.
    pub fn last_outcome(&self) -> Option<TurnOutcome> {
        self.finisher.last_outcome()
    }

    /// A receiver that changes on every turn boundary and every turn start, so `wait_task`/
    /// `wait_any` wake without polling.
    pub fn subscribe_turns(&self) -> watch::Receiver<u64> {
        self.finisher.subscribe_turns()
    }
}

/// What to spawn. The registry fills the rest from its own configuration.
#[derive(Clone)]
pub struct SpawnRequest {
    /// The task id: `[A-Za-z0-9._-]`, 1-128 chars (it names a worktree directory and a branch).
    pub id: String,
    /// How to isolate the task (ADR 0030 O3). `None` lets the agent definition's `isolation`
    /// decide, then the `worktree` default.
    pub isolation: Option<Isolation>,
    pub label: Option<String>,
    /// The first input, sent as a `followup` with `InputSource::Subagent` (ADR 0030 O2: a task
    /// agent's every input is `Subagent`). `None` opens the session without starting a turn.
    pub brief: Option<String>,
    /// The task's approval policy after the hard-deny list and the edge policy (ADR 0030
    /// revision 1 R1): `None` lets the agent definition's `approval`, then the task default
    /// `auto`, decide. `Some(OnAsk)` sends every ask that still reaches the default to the
    /// task-side pending queue (ADR 0030 slice 5). `Never` and `Always` are refused: a task has
    /// no "never ask" and no "always ask" layer of its own.
    pub approval: Option<ApprovalPolicy>,
    /// The optional edge policy command (ADR 0030 R1 layer 2). `None` means no edge
    /// policy: the default in [`SpawnRequest::approval`] decides.
    ///
    /// Trust: this command must come from the **user's own config** or a **trusted project
    /// config** (ADR 0030 slice 9 / ADR 0031), never from a model tool argument — it is a
    /// program Radian runs for the task. It is only ever run inside the task's sandbox; with no
    /// sandbox it is skipped and the request goes to the pending queue.
    pub policy: Option<crate::task_pending::PolicyCommand>,
    /// The ADR 0030 O11 agent definition name; `None` means the task default (the `task` role).
    pub agent: Option<String>,
    /// A `provider/model` route (ADR 0026) that overrides the definition's `model`.
    pub model: Option<String>,
    /// A tier name that overrides the definition's `profile`. A task may only narrow.
    pub profile: Option<String>,
}

impl SpawnRequest {
    /// A worktree task with no brief (the caller sends one, or drives it in a test).
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            isolation: None,
            label: None,
            brief: None,
            approval: None,
            policy: None,
            agent: None,
            model: None,
            profile: None,
        }
    }
}

/// What a start produced: a running task, or a queued one with its 1-based queue position.
#[derive(Clone)]
pub struct Spawned {
    pub id: String,
    /// The started record, or `None` while the task is queued.
    pub record: Option<Arc<TaskRecord>>,
    /// The 1-based queue position, `Some` only while queued.
    pub position: Option<usize>,
}

/// The agent definition resolved **and snapshotted at `spawn_task`** (ADR 0030 item 4):
/// carried through the `max_tasks` queue so a queued task never re-reads the file when it starts.
#[derive(Clone)]
struct ResolvedAgent {
    agent: SpawnAgent,
    isolation: Isolation,
    approval: ApprovalPolicy,
}

/// A task accepted but not yet started: over `max_tasks`, it waits FIFO (ADR 0030 O4/O8).
pub struct QueuedTask {
    id: String,
    request: SpawnRequest,
    /// The definition snapshot taken at submit time (ADR 0030 item 4).
    resolved: ResolvedAgent,
    state: Mutex<QueuedState>,
}

enum QueuedState {
    /// Waiting for a slot.
    Waiting,
    /// Its start failed; the message is why, and a waiting lead gets it.
    Failed(String),
    /// Removed while queued: it never started and wrote no `TaskSpawned`.
    Removed,
}

impl QueuedTask {
    fn new(request: SpawnRequest, resolved: ResolvedAgent) -> Self {
        Self {
            id: request.id.clone(),
            request,
            resolved,
            state: Mutex::new(QueuedState::Waiting),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn label(&self) -> Option<&str> {
        self.request.label.as_deref()
    }

    pub fn isolation(&self) -> Isolation {
        self.resolved.isolation
    }

    /// The start failure, if one happened.
    pub fn failed(&self) -> Option<String> {
        match &*self.state.lock().unwrap_or_else(|e| e.into_inner()) {
            QueuedState::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }

    /// Whether it was removed before starting.
    pub fn is_removed(&self) -> bool {
        matches!(
            &*self.state.lock().unwrap_or_else(|e| e.into_inner()),
            QueuedState::Removed
        )
    }

    fn set(&self, state: QueuedState) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }
}

/// One task as the lead sees it: started (a record) or queued (a ticket).
pub enum TaskHandle {
    Started(Arc<TaskRecord>),
    Queued(Arc<QueuedTask>),
}

/// One `stalled` notification (ADR 0030 O8/R5): a task ran for `idle_secs` with no committed
/// event and no successful write. Raised once per episode; a new event or write ends the
/// episode and a later quiet stretch can raise a new one. The runtime never intervenes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallNotice {
    pub task_id: String,
    /// Seconds since the last committed event or successful write (the task's start when it
    /// has had neither).
    pub idle_secs: u64,
}

/// The task agents' worktree root (ADR 0039 decision 4): `<radian-home>/worktrees/<repo-key>`,
/// plus the radian home the symlink check starts from.
#[derive(Debug, Clone)]
pub(crate) struct WorktreeRoot {
    pub(crate) home: PathBuf,
    pub(crate) root: PathBuf,
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
    /// The lead's journal: `TaskSpawned`/`TaskFinished` land here (ADR 0030 O5).
    lead_journal: Arc<SessionJournal>,
    /// The task-side pending queue (ADR 0030 slice 5): every task's ask-still-ask and
    /// `ask_user`, waiting for the lead, plus the human queue an escalation reaches.
    pending: Arc<TaskPendingQueue>,
    builder: TaskBuilder,
    /// Test seams (ADR 0039 decisions 5 and 7); both are `None` in production.
    seams: TaskSeams,
    /// The host's notice callback for `TaskSpawned`/`TaskFinished` (ADR 0030 O10); `None` in
    /// tests. See [`TaskRegistry::with_notifier`].
    notifier: Option<TaskNotifier>,
    tasks: Mutex<BTreeMap<String, Arc<TaskRecord>>>,
    /// The orchestration limits (ADR 0030 O8): `max_tasks` and `stall_after_secs`.
    limits: OrchestrationLimits,
    /// The process-level heavy gate (ADR 0030 O8). The registry installs it with
    /// [`HeavySlots::set_process`] at construction, so the lead's and every task's shell share
    /// one pool; `heavy_slots()` hands it to the session builder.
    heavy: Arc<HeavySlots>,
    /// Tasks over `max_tasks`, in FIFO order (ADR 0030 O4/O8).
    queue: Mutex<VecDeque<String>>,
    /// The queued tasks by id, so a status lookup and `remove` can find one.
    queued_tasks: Mutex<BTreeMap<String, Arc<QueuedTask>>>,
    /// Bumped on every observable change (a task starts, a queued task is dropped, a turn
    /// boundary, a stall). A `wait_task`/`wait_any` wakes on it instead of polling.
    wake: watch::Sender<u64>,
    /// The FIFO pump's wake: raised when a slot frees or a task is queued.
    pump: Arc<Notify>,
    /// Whether the pump task has been started (once, lazily, on the first queued task).
    pump_started: AtomicBool,
    /// Serialises start decisions so a queued start and a fresh `spawn_task` cannot both pass
    /// the `max_tasks` check (an async mutex: it is held across `open_session`).
    scheduler: tokio::sync::Mutex<()>,
    /// Bumped when a stall notice is pushed, so a host watch (b2's TUI) wakes.
    stall_version: watch::Sender<u64>,
    /// The stall notices not yet taken, in order.
    stalls: Mutex<VecDeque<StallNotice>>,
    /// Per-task stall-episode state (ADR 0030 R5): `true` while a notice is outstanding.
    stall_episode: Mutex<BTreeMap<String, bool>>,
    /// The per-task read-only observers the stall check reads (ADR 0030 item 10:
    /// the timestamps come from the observer). Opened lazily.
    observers: Mutex<BTreeMap<String, JournalObserver>>,
    /// The wall clock, injectable so a test never sleeps 600 s.
    clock: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
    /// Whether `clock` was injected (`with_clock`). A manual clock means a test drives
    /// [`TaskRegistry::check_stalls`]; the ticker is then off, so it cannot consume a notice the
    /// test wanted to observe.
    manual_clock: bool,
}

impl TaskRegistry {
    /// Builds the registry over the lead's workspace and session, with the ADR 0030 O8 defaults
    /// ([`OrchestrationLimits::default`]).
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
        let pending = TaskPendingQueue::new(Some(Arc::clone(&lead_queue)));
        let limits = OrchestrationLimits::default();
        // Install the process gate from these limits (ADR 0030 O8). First configuration wins:
        // the process reads its orchestration config once (ADR 0030 slice 9).
        HeavySlots::set_process(limits.heavy_slots, limits.heavy_patterns.clone());
        let heavy = HeavySlots::process();
        let (wake, _) = watch::channel(0u64);
        let (stall_version, _) = watch::channel(0u64);
        Self {
            workspace,
            worktree_root,
            lead_session_id: lead_session_id.into(),
            lead_journal,
            pending,
            builder,
            seams: TaskSeams::default(),
            notifier: None,
            tasks: Mutex::new(BTreeMap::new()),
            limits,
            heavy,
            queue: Mutex::new(VecDeque::new()),
            queued_tasks: Mutex::new(BTreeMap::new()),
            wake,
            pump: Arc::new(Notify::new()),
            pump_started: AtomicBool::new(false),
            scheduler: tokio::sync::Mutex::new(()),
            stall_version,
            stalls: Mutex::new(VecDeque::new()),
            stall_episode: Mutex::new(BTreeMap::new()),
            observers: Mutex::new(BTreeMap::new()),
            clock: Box::new(Utc::now),
            manual_clock: false,
        }
    }

    /// Installs the test seams (ADR 0039 decisions 5 and 7).
    pub fn with_seams(mut self, seams: TaskSeams) -> Self {
        self.seams = seams;
        self
    }

    /// Sets the orchestration limits (ADR 0030 O8). The heavy gate is rebuilt and installed
    /// with [`HeavySlots::set_process`].
    pub fn with_limits(mut self, limits: OrchestrationLimits) -> Self {
        HeavySlots::set_process(limits.heavy_slots, limits.heavy_patterns.clone());
        self.heavy = HeavySlots::process();
        self.limits = limits;
        self
    }

    /// Installs a test clock (ADR 0030 R5 uses time; a test never sleeps 600 s). With an
    /// injected clock the stall ticker is **off**: the test drives
    /// [`TaskRegistry::check_stalls`], so the ticker cannot consume a notice first.
    pub fn with_clock(mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self.manual_clock = true;
        self
    }

    /// The configured `max_tasks` (ADR 0030 O8).
    pub fn max_tasks(&self) -> usize {
        self.limits.max_tasks
    }

    /// The configured `stall_after_secs` (ADR 0030 O8/R5).
    pub fn stall_after_secs(&self) -> f64 {
        self.limits.stall_after_secs
    }

    /// The process-level heavy gate this registry's sessions share (ADR 0030 O8).
    pub fn heavy_slots(&self) -> &Arc<HeavySlots> {
        &self.heavy
    }

    /// A receiver that changes on every task state change (a start, a turn boundary, a removal,
    /// a stall). The lead's `wait_*` waits on it instead of polling.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    fn bump_wake(&self) {
        self.wake
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Installs the host's notice callback for `TaskSpawned`/`TaskFinished` (ADR 0030 O10). The
    /// two hosts pass the lead's live runtime-events sink here, so a spawn and a finish become
    /// notice lines in the lead's transcript; tests pass none.
    pub fn with_notifier(mut self, notifier: TaskNotifier) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Calls the host's notice callback, when one is installed.
    fn notify(&self, event: &RuntimeEvent) {
        if let Some(notifier) = &self.notifier {
            notifier(event);
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

    /// The task-side pending queue (ADR 0030 slice 5). A lead's `wait_task`/`wait_any` (slice 6)
    /// takes the next event with [`TaskRegistry::pending_next`].
    pub fn pending(&self) -> &Arc<TaskPendingQueue> {
        &self.pending
    }

    /// Takes the next pending task event (a question or a still-ask approval) and starts its R9
    /// timeout (ADR 0030). `None` when no task is waiting.
    pub fn pending_next(&self) -> Option<TaskPendingView> {
        self.pending.next()
    }

    /// [`TaskRegistry::pending_next`] restricted to one task (ADR 0030 slice 6's `wait_task`):
    /// the lead's wait for `id` never takes another task's event.
    pub fn pending_next_for(&self, id: &str) -> Option<TaskPendingView> {
        self.pending.next_for(id)
    }

    /// [`TaskRegistry::pending_next`] restricted to a set of tasks (ADR 0030 slice 6's
    /// `wait_any { ids }`): the first event raised by any of them, in arrival order.
    pub fn pending_next_in(&self, ids: &[String]) -> Option<TaskPendingView> {
        self.pending.next_in(ids)
    }

    /// Sets the R9 timeout applied to each taken event (ADR 0030).
    pub fn set_pending_timeout(&self, timeout: std::time::Duration) {
        self.pending.set_timeout(timeout);
    }

    /// Answers a task's `ask_user` question (ADR 0030 R9 item 1).
    pub fn answer(
        &self,
        task_id: &str,
        request_id: &str,
        answers: Vec<radian_coding::QuestionReply>,
    ) -> Result<(), TaskPendingError> {
        self.pending.answer(task_id, request_id, answers)
    }

    /// Refuses a task's pending approval. This only ever narrows (ADR 0030 R9 item 2):
    /// there is deliberately no operation that approves an approval on the lead's behalf.
    pub fn deny_approval(
        &self,
        task_id: &str,
        request_id: &str,
        reason: Option<String>,
    ) -> Result<(), TaskPendingError> {
        self.pending.deny_approval(task_id, request_id, reason)
    }

    /// Hands a task's pending approval to the human queue with the lead's summary and
    /// recommendation (ADR 0030 R9 item 2). The human's answer resolves the task.
    pub fn escalate_approval(
        &self,
        task_id: &str,
        request_id: &str,
        summary: &str,
        recommendation: &str,
    ) -> Result<(), TaskPendingError> {
        self.pending
            .escalate_approval(task_id, request_id, summary, recommendation)
    }

    /// One task by id — started or queued — or `None` when there is no such task.
    pub fn lookup(&self, id: &str) -> Option<TaskHandle> {
        if let Some(record) = self.get(id) {
            return Some(TaskHandle::Started(record));
        }
        self.queued_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .map(TaskHandle::Queued)
    }

    /// Every queued task's id and 1-based queue position, in FIFO order.
    pub fn queued(&self) -> Vec<(String, usize)> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index + 1))
            .collect()
    }

    /// The 1-based queue position of `id`, or `None` when it is not queued.
    pub fn queued_position(&self, id: &str) -> Option<usize> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .position(|queued| queued == id)
            .map(|index| index + 1)
    }

    /// How many tasks occupy a `max_tasks` slot: started and not yet finished a turn (or
    /// removed). A task that has had a turn end frees its slot (ADR 0030 O4: a queued task
    /// starts when a running one finishes), as does `remove`.
    pub fn live_count(&self) -> usize {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|record| record.last_outcome().is_none())
            .count()
    }

    /// Starts a task, or queues it FIFO when `max_tasks` are already running (ADR 0030 O4/O8).
    /// `spawn_task` calls this; the tests and the FIFO pump call [`TaskRegistry::spawn`] directly.
    pub fn submit(
        self: &Arc<Self>,
        request: SpawnRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<Spawned, TaskError>> {
        Box::pin(async move {
            let id = request.id.clone();
            validate_session_id(&id).map_err(|_| {
                TaskError::new(format!(
                    "invalid task id {id:?}: [A-Za-z0-9._-], 1-128 chars"
                ))
            })?;
            self.ensure_pump();
            // The scheduler lock is held across the start so a fresh spawn and the pump cannot
            // both pass the cap check.
            let _guard = self.scheduler.lock().await;
            if self.get(&id).is_some()
                || self
                    .queued_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&id)
            {
                return Err(TaskError::new(format!("task `{id}` already exists")));
            }
            // ADR 0030 item 4: snapshot the definition **here**, at the `spawn_task`
            // call, and carry it through the queue. A queued task must not re-read the file (or
            // re-resolve against a later edit) when a slot frees; and a bad definition is the
            // caller's error now, not a queued failure.
            let resolved = resolve_task(&request)?;
            if self.live_count() < self.limits.max_tasks {
                let record = self.spawn_resolved(request, resolved).await?;
                self.bump_wake();
                return Ok(Spawned {
                    id,
                    record: Some(record),
                    position: None,
                });
            }
            let queued = Arc::new(QueuedTask::new(request, resolved));
            {
                self.queued_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id.clone(), Arc::clone(&queued));
                self.queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(id.clone());
            }
            let position = self.queued_position(&id);
            self.bump_wake();
            Ok(Spawned {
                id,
                record: None,
                position,
            })
        })
    }

    /// Starts the FIFO pump and the stall ticker once (ADR 0030 O4/O8). Both hold a `Weak`, so
    /// they never keep the registry alive.
    fn ensure_pump(self: &Arc<Self>) {
        if self.pump_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime: nothing async can be dispatched. A synchronous caller that needs the
            // queue must have a runtime; tests and both hosts do.
            self.pump_started.store(false, Ordering::SeqCst);
            return;
        };
        let weak = Arc::downgrade(self);
        let pump = Arc::clone(&self.pump);
        handle.spawn(async move {
            loop {
                pump.notified().await;
                let Some(registry) = weak.upgrade() else {
                    break;
                };
                registry.dispatch().await;
            }
        });
        // The stall ticker (ADR 0030 O8/R5): checks live tasks every `min(stall / 4, 30 s)` so a
        // notice lands close to the threshold. A zero threshold disables it; so does an injected
        // clock (a test drives `check_stalls` itself).
        if self.limits.stall_after_secs > 0.0 && !self.manual_clock {
            let interval = Duration::from_secs_f64((self.limits.stall_after_secs / 4.0).min(30.0));
            let weak = Arc::downgrade(self);
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let Some(registry) = weak.upgrade() else {
                        break;
                    };
                    registry.check_stalls();
                }
            });
        }
    }

    /// A slot may have freed (a turn ended, a task was removed): wake the pump.
    fn notify_slot(&self) {
        self.pump.notify_one();
    }

    /// Starts queued tasks while `max_tasks` slots are free, FIFO (ADR 0030 O4/O8). Holds the
    /// scheduler lock so a concurrent `spawn_task` cannot race the cap.
    async fn dispatch(self: &Arc<Self>) {
        let _guard = self.scheduler.lock().await;
        loop {
            if self.live_count() >= self.limits.max_tasks {
                break;
            }
            let id = self
                .queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            let Some(id) = id else {
                break;
            };
            let queued = self
                .queued_tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned();
            let Some(queued) = queued else {
                continue; // removed between the pop and here
            };
            let request = queued.request.clone();
            let resolved = queued.resolved.clone();
            // The queued ticket is no longer in `tasks`, so `spawn_resolved`'s duplicate check is
            // clear. `resolved` is the snapshot taken at submit (ADR 0030 item 4).
            match self.spawn_resolved(request, resolved).await {
                Ok(_record) => {
                    // Started: drop the ticket; `get(id)` now finds the record.
                    self.queued_tasks
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
                Err(error) => {
                    // Keep the ticket with the failure so a waiting lead gets it; it is out of
                    // the FIFO order already (popped above), so it blocks nothing.
                    queued.set(QueuedState::Failed(error.to_string()));
                }
            }
            self.bump_wake();
            self.notify_slot();
        }
    }

    /// The stall notices raised but not yet taken, in order (ADR 0030 O8/R5).
    pub fn subscribe_stalls(&self) -> watch::Receiver<u64> {
        self.stall_version.subscribe()
    }

    /// Takes the next stall notice (ADR 0030 O8/R5). Mirrors the pending watch: the host
    /// subscribes to [`TaskRegistry::subscribe_stalls`] and takes notices here.
    pub fn next_stall(&self) -> Option<StallNotice> {
        self.stalls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }

    /// Checks every live (not-yet-finished) task against `stall_after_secs` and raises one notice
    /// per episode (ADR 0030 O8/R5). Called by the ticker in production and directly by a test
    /// with a clock seam; returns how many notices it raised this check.
    ///
    /// A task is stalled when its **last activity** — the more recent of its last committed
    /// event and its last successful `write`/`patch` (ADR 0030 R5's two observer timestamps) —
    /// is older than the threshold; the task's start is the reference before either exists. A
    /// new event or write clears the episode, and a later quiet stretch raises a new one. The
    /// runtime never cancels the task. A task that has finished its turn is cleared and skipped:
    /// it is done, not stuck.
    pub fn check_stalls(&self) -> usize {
        let now = (self.clock)();
        let records: Vec<Arc<TaskRecord>> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut fired = 0;
        for record in records {
            // A task removed between the snapshot and here is gone; do not raise for it.
            if self.get(&record.id).is_none() {
                continue;
            }
            if record.last_outcome().is_some() {
                // A finished task is not making progress toward nothing; clear any episode so a
                // later turn can raise a new one. (A task's turn state is "has it finished",
                // which the runtime records; `running()` is not wired to `TurnStarted`.)
                self.clear_stall(&record.id);
                continue;
            }
            let activity = self.last_activity(&record);
            let idle = now.signed_duration_since(activity);
            let stalled = idle.num_milliseconds() as f64 >= self.limits.stall_after_secs * 1000.0;
            if stalled {
                if !self.stall_active(&record.id) {
                    self.mark_stall(&record.id);
                    let notice = StallNotice {
                        task_id: record.id.clone(),
                        idle_secs: idle.num_seconds().max(0) as u64,
                    };
                    self.stalls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(notice);
                    self.stall_version
                        .send_modify(|version| *version = version.wrapping_add(1));
                    fired += 1;
                }
            } else {
                self.clear_stall(&record.id);
            }
        }
        fired
    }

    /// The task's last activity: the later of its last committed event, its last successful
    /// write, and its start. Reads the committed facts through the observer (ADR 0030 revision
    /// 5 item 10).
    fn last_activity(&self, record: &Arc<TaskRecord>) -> DateTime<Utc> {
        let mut latest = record.started_at;
        let mut observers = self.observers.lock().unwrap_or_else(|e| e.into_inner());
        if !observers.contains_key(&record.id)
            && let Ok(observer) = JournalObserver::open(record.session.journal_path())
        {
            observers.insert(record.id.clone(), observer);
        }
        if let Some(observer) = observers.get_mut(&record.id) {
            let _ = observer.poll();
            for ts in [observer.last_event_ts(), observer.last_write_ts()]
                .into_iter()
                .flatten()
            {
                if let Ok(parsed) = DateTime::parse_from_rfc3339(ts) {
                    latest = latest.max(parsed.with_timezone(&Utc));
                }
            }
        }
        latest
    }

    fn stall_active(&self, id: &str) -> bool {
        self.stall_episode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
            .unwrap_or(false)
    }

    fn mark_stall(&self, id: &str) {
        self.stall_episode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), true);
    }

    fn clear_stall(&self, id: &str) {
        self.stall_episode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// Spawns a task agent (ADR 0030 O2/O3) as one transaction (ADR 0039 decision 5).
    ///
    /// Steps, in order: resolve and snapshot the agent definition (ADR 0030 item 4),
    /// create the worktree, append the lead's `TaskSpawned`, build and open the task session with
    /// the enforced invariants, and (if a brief was given) send it as a `Subagent` input. A
    /// failure at any step after the worktree exists removes it again; a failure after
    /// `TaskSpawned` was written also appends the paired `TaskFinished`.
    /// `send_task`'s future is **boxed** (a type-erased `BoxFuture`): `spawn` calls `open_session`,
    /// and a lead tool that calls `open_session` would otherwise recurse in the async `Send`
    /// inference (open_session's driver runs the tools, one of which is `spawn_task`).
    pub fn spawn(
        &self,
        request: SpawnRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<Arc<TaskRecord>, TaskError>> {
        Box::pin(async move {
            let resolved = resolve_task(&request)?;
            self.spawn_resolved(request, resolved).await
        })
    }

    /// The spawn transaction after the definition is resolved (ADR 0030 O2/O3, ADR 0039
    /// decision 5). [`TaskRegistry::spawn`] resolves then calls this; [`TaskRegistry::submit`]
    /// resolves at the `spawn_task` call and calls this both immediately and from the FIFO pump,
    /// so a queued task starts from its submit-time snapshot.
    fn spawn_resolved(
        &self,
        request: SpawnRequest,
        resolved: ResolvedAgent,
    ) -> futures_util::future::BoxFuture<'_, Result<Arc<TaskRecord>, TaskError>> {
        Box::pin(async move {
            let SpawnRequest {
                id,
                label,
                brief,
                policy,
                ..
            } = request;
            validate_session_id(&id).map_err(|_| {
                TaskError::new(format!(
                    "invalid task id {id:?}: [A-Za-z0-9._-], 1-128 chars"
                ))
            })?;
            if self.get(&id).is_some() {
                return Err(TaskError::new(format!("task `{id}` already exists")));
            }
            let ResolvedAgent {
                agent,
                isolation,
                approval,
            } = resolved;

            // The guard owns everything the spawn has built so far; it is disarmed only once the
            // record is in the registry, so no failure below can leave a worktree or a driver.
            let mut guard = SpawnGuard::new(self.workspace.clone());
            match self
                .spawn_inner(
                    &mut guard,
                    &id,
                    isolation,
                    label,
                    brief,
                    (approval, policy),
                    agent,
                )
                .await
            {
                Ok(record) => {
                    guard.disarm();
                    Ok(record)
                }
                Err(error) => {
                    // ADR 0039 decision 5: the `TaskSpawned` this spawn wrote needs a pair.
                    if guard.spawned {
                        let event = RuntimeEvent::TaskFinished {
                            agent_session_id: id.clone(),
                            outcome: TurnOutcome::Failed,
                            error: Some(error.to_string()),
                        };
                        let _ = self.lead_journal.append_runtime_event(event.clone()).await;
                        self.notify(&event);
                    }
                    guard.rollback().await;
                    Err(error)
                }
            }
        })
    }

    /// The fallible body of [`TaskRegistry::spawn`]; the caller owns the guard.
    #[allow(clippy::too_many_arguments)] // the caller (spawn) destructures the request already
    async fn spawn_inner(
        &self,
        guard: &mut SpawnGuard,
        id: &str,
        isolation: Isolation,
        label: Option<String>,
        brief: Option<String>,
        // The task's approval policy and its optional edge policy command (ADR 0030 slice 5),
        // together: they are only ever read as a pair by the task approver below.
        (approval, policy): (ApprovalPolicy, Option<crate::task_pending::PolicyCommand>),
        agent: SpawnAgent,
    ) -> Result<Arc<TaskRecord>, TaskError> {
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
                let worktree = root.root.join(id);
                let lead = self.workspace.clone();
                let home = root.home.clone();
                let wt = worktree.clone();
                let br = format!("radian/{id}");
                let br_for_git = br.clone();
                tokio::task::spawn_blocking(move || {
                    create_worktree(&lead, &home, &wt, &br_for_git)
                })
                .await
                .map_err(|error| TaskError::new(format!("worktree task failed: {error}")))??;
                guard.worktree = Some(worktree.clone());
                (worktree.clone(), Some(worktree), Some(br))
            }
            Isolation::Shared => (self.workspace.clone(), None, None),
        };

        // The branch point, for a later `git diff --stat` relative to the start (ADR 0030 R8).
        // A failed read leaves it `None`; `list_tasks` then omits the diff rather than inventing one.
        let start_commit = match &worktree {
            Some(wt) => {
                let wt = wt.clone();
                tokio::task::spawn_blocking(move || git_head(&wt))
                    .await
                    .ok()
                    .flatten()
            }
            None => None,
        };

        // 2. The lead's `TaskSpawned`, before the task session exists. `parent.seq` is its seq.
        self.seam(SpawnStage::TaskSpawned)?;
        let start_seq = self.lead_journal.next_seq();
        let spawned = RuntimeEvent::TaskSpawned {
            agent_session_id: id.to_owned(),
            worktree: worktree
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            label: label.clone(),
        };
        self.lead_journal
            .append_runtime_event(spawned.clone())
            .await
            .map_err(|error| TaskError::new(format!("cannot write TaskSpawned: {error}")))?;
        // Tell the host's live view (ADR 0030 O10). After the write, so a host that reacts by
        // reading the lead's journal sees the record.
        self.notify(&spawned);
        guard.spawned = true;
        let parent = ForkParent::task(self.lead_session_id.clone(), start_seq);

        // 3. Build and open the session, enforcing the task invariants on what the builder gave.
        self.seam(SpawnStage::Builder)?;
        let parts = (self.builder)(&TaskContext {
            id,
            session_id: id,
            isolation,
            workspace: &workspace,
            branch: branch.as_deref(),
            parent: &parent,
            agent: Some(&agent),
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
        self.seam(SpawnStage::Enforce)?;
        enforce_task_setup(
            &mut setup,
            &workspace,
            isolation,
            branch.clone(),
            parent.clone(),
            // The hook consults the approver whenever the task's own layers might decide
            // something: an edge policy runs there, and `on-ask` routes there. Only a plain
            // `auto` task with no edge policy lets the hook answer `ask` with `allow` itself
            // (ADR 0030 R1: hard-deny -> edge policy -> auto).
            if approval == ApprovalPolicy::OnAsk || policy.is_some() {
                ApprovalPolicy::OnAsk
            } else {
                ApprovalPolicy::Auto
            },
            Some(&agent),
        )?;

        // The task's asks and questions go to the task-side pending queue, not the human's; the
        // task is committed when its turn ends (ADR 0030 slice 5).
        let task_approver = Arc::new(TaskApprover::new(
            Arc::clone(&self.pending),
            id.to_owned(),
            approval,
            policy,
            setup.sandbox.clone(),
        ));
        let task_prompter = Arc::new(TaskPrompter::new(Arc::clone(&self.pending), id.to_owned()));
        bindings.shared_queue = None;
        bindings.approvals = ApprovalRouting::Task {
            approver: task_approver,
            prompter: task_prompter,
        };
        // ADR 0030 R9: a task agent's questions go to the lead, so `ask_user` is
        // advertised for the task even when the lead's own host did not advertise it to itself.
        bindings.ask_user = true;
        let finisher = Arc::new(TaskFinisher {
            id: id.to_owned(),
            lead_journal: Arc::clone(&self.lead_journal),
            worktree: worktree.clone(),
            branch: branch.clone(),
            protected,
            filters,
            pin: worktree.as_ref().and_then(|path| RepoPin::resolve(path)),
            journal: OnceLock::new(),
            runtime: tokio::runtime::Handle::current(),
            gate: self.seams.commit_gate.clone(),
            notifier: self.notifier.clone(),
            state: Mutex::new(CommitState::Idle),
            done: watch::channel(false).0,
            running: AtomicBool::new(false),
            last_outcome: Mutex::new(None),
            turns: watch::channel(0u64).0,
            // ADR 0030 O8: a turn end frees a `max_tasks` slot, so wake the FIFO pump.
            notify: Arc::clone(&self.pump),
            finished: AtomicBool::new(false),
        });
        let inner = Arc::clone(&bindings.runtime_events);
        bindings.runtime_events = Arc::new(TaskRuntimeEvents {
            inner,
            finisher: Arc::clone(&finisher),
            wake: self.wake.clone(),
        });

        // The sandbox policy the task's tools will run under (kept for the record): read it
        // before `open_session` consumes the setup.
        let sandbox_policy = setup
            .sandbox
            .as_ref()
            .map(|sandbox| sandbox.policy().clone());
        let session = Arc::new(
            open_session(setup, bindings)
                .await
                .map_err(|error| TaskError::new(error.to_string()))?,
        );
        guard.session = Some(Arc::clone(&session));
        let _ = finisher.journal.set(Arc::clone(session.journal()));
        // ADR 0030 O2 / ADR 0012 decision 5: every input a task agent receives is `Subagent`,
        // so its turn source is `Subagent` — never the human's, and it can borrow no session
        // grant. Set once: nothing else feeds a task's inbox.
        session
            .hooks()
            .set_turn_source(Some(agent_core::InputSource::Subagent));

        // 4. The brief, as a `Subagent` input (ADR 0030 O2).
        if let Some(brief) = brief {
            self.seam(SpawnStage::Brief)?;
            session
                .inbox()
                .followup(Message::user(brief), agent_core::InputSource::Subagent)
                .await
                .map_err(|error| TaskError::new(format!("cannot send the brief: {error}")))?;
        }

        // ADR 0030 O11: a `skills` name discovery did not find is a spawn-time warning, never a
        // failure. The session's own catalog is already filtered to the allow-list, so the
        // names still missing there are exactly the ones to warn about.
        let warnings = match &agent.skills {
            Some(allow) => crate::skills::missing(session.skills(), allow),
            None => Vec::new(),
        };

        let record = Arc::new(TaskRecord {
            id: id.to_owned(),
            session,
            isolation,
            worktree,
            branch,
            label,
            agent: agent.name.clone(),
            spawn_ignored: agent.spawn_ignored,
            warnings,
            start_seq,
            start_commit,
            sandbox_policy,
            started_at: Utc::now(),
            finisher,
        });
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), Arc::clone(&record));
        Ok(record)
    }

    /// Runs the spawn test seam, if this registry has one (ADR 0039 decision 5).
    fn seam(&self, stage: SpawnStage) -> Result<(), TaskError> {
        match &self.seams.spawn {
            Some(hook) => hook(stage).map_err(TaskError::new),
            None => Ok(()),
        }
    }

    /// Closes a task and removes its worktree (ADR 0030 O3: `remove_task`).
    ///
    /// The session is stopped and its driver joined first, so nothing writes while the worktree
    /// goes; then the end-of-task commit is awaited (ADR 0039 decision 7), so a commit still in
    /// flight is never removed under. Then, unless `force`, a dirty worktree is refused
    /// (uncommitted changes are the user's to look at; after a normal end-of-task commit the
    /// worktree is clean). `force` removes it regardless. The record is kept when the removal
    /// is refused.
    pub async fn remove(&self, id: &str, force: bool) -> Result<(), TaskError> {
        // The scheduler lock keeps a queued-task removal from interleaving with the pump's
        // dispatch (which holds it across `spawn`): without it, the pump could observe the
        // ticket, then a concurrent removal sets it `Removed`, and the pump still starts it.
        let _guard = self.scheduler.lock().await;
        // A queued task has no session and no worktree: dropping it from the queue is all
        // (ADR 0030 O4). It wrote no `TaskSpawned`, so nothing needs pairing.
        if let Some(queued) = self
            .queued_tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
        {
            self.queue
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|queued| queued != id);
            queued.set(QueuedState::Removed);
            self.clear_stall(id);
            self.bump_wake();
            self.notify_slot();
            return Ok(());
        }
        let record = self
            .get(id)
            .ok_or_else(|| TaskError::new(format!("no task `{id}`")))?;
        record.session.stop();
        let _ = record.session.join().await;
        // ADR 0039 decision 7: the end-of-task commit lands (or fails) before the worktree goes.
        let end = record.await_end_commit().await;
        // ADR 0030 O5: a task removed before any turn ended still writes its one
        // `TaskFinished`, `cancelled`. A task that ended wrote it at its turn end instead, so
        // `end` is `Some` and this is skipped (`write_finished` is once-only as well).
        if end.is_none() {
            record
                .finisher
                .write_finished(TurnOutcome::Cancelled, None)
                .await;
        }
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
        self.observers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.clear_stall(id);
        // Removing frees a `max_tasks` slot (ADR 0030 O4): wake the FIFO pump.
        self.bump_wake();
        self.notify_slot();
        Ok(())
    }

    /// Closes a task's session and abandons its pending requests, **keeping** its worktree
    /// (ADR 0030 O12's `/tasks kill`). Unlike [`TaskRegistry::remove`], nothing is deleted from
    /// disk: the worktree and its branch stay for the human to inspect.
    ///
    /// The session is stopped and its driver joined first, so the task's blocked pending calls
    /// fail; the queue drops an abandoned entry on its next read. The record leaves the registry,
    /// so a later `wait_task`/`task_status` sees no such task. A task killed before any turn
    /// ended writes its one `TaskFinished`, `cancelled` (ADR 0030 O5), so the kill does not read
    /// as a crash on resume.
    pub async fn kill(&self, id: &str) -> Result<(), TaskError> {
        let record = self
            .get(id)
            .ok_or_else(|| TaskError::new(format!("no task `{id}`")))?;
        record.session.stop();
        let _ = record.session.join().await;
        // ADR 0039 decision 7: a commit already in flight settles first; ADR 0030 O5: no turn
        // ended, so the kill is the task's end and writes the one `TaskFinished`, `cancelled`.
        let end = record.await_end_commit().await;
        if end.is_none() {
            record
                .finisher
                .write_finished(TurnOutcome::Cancelled, None)
                .await;
        }
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.observers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        self.clear_stall(id);
        // The kill frees a `max_tasks` slot (ADR 0030 O4): wake the FIFO pump.
        self.bump_wake();
        self.notify_slot();
        Ok(())
    }
}

/// Owns a half-built task during [`TaskRegistry::spawn`] (ADR 0039 decision 5): its worktree
/// from the moment it exists, and — once `open_session` returned — the session's driver. The
/// registry disarms it only after the record is in the registry; any earlier failure rolls it
/// back, and a dropped spawn (cancellation) is rolled back by `Drop`.
struct SpawnGuard {
    lead: PathBuf,
    worktree: Option<PathBuf>,
    session: Option<Arc<LocalSession>>,
    /// Whether the lead's `TaskSpawned` was written; a later failure pairs a `TaskFinished`.
    spawned: bool,
    armed: bool,
}

impl SpawnGuard {
    fn new(lead: PathBuf) -> Self {
        Self {
            lead,
            worktree: None,
            session: None,
            spawned: false,
            armed: true,
        }
    }

    /// The spawn reached the registry: nothing is ours to clean up any more.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Stops and joins the task's driver, then removes its worktree.
    async fn rollback(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if let Some(session) = self.session.take() {
            session.stop();
            let _ = session.join().await;
        }
        if let Some(worktree) = self.worktree.take() {
            let lead = self.lead.clone();
            let _ =
                tokio::task::spawn_blocking(move || remove_worktree(&lead, &worktree, true)).await;
        }
    }
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A dropped `spawn` future: the driver is aborted at once and the worktree is removed
        // with native git (no runtime to await on here).
        if let Some(session) = self.session.take() {
            session.abort();
        }
        if let Some(worktree) = self.worktree.take() {
            let _ = remove_worktree(&self.lead, &worktree, true);
        }
    }
}

/// Enforces the task-agent invariants on a builder's setup (ADR 0030 O2/O3/O8, ADR 0036,
/// ADR 0038 decision 4, ADR 0030 R1, ADR 0030 O11). Split out so a test can pin each
/// rule without a real session.
#[allow(clippy::too_many_arguments)] // one enforcer over every task invariant, not a builder
fn enforce_task_setup(
    setup: &mut SessionSetup,
    workspace: &Path,
    isolation: Isolation,
    branch: Option<String>,
    parent: ForkParent,
    hook_approval: ApprovalPolicy,
    agent: Option<&SpawnAgent>,
) -> Result<(), TaskError> {
    setup.workspace = workspace.to_path_buf();
    setup.coding_root = Some(workspace.to_path_buf());
    setup.parent = Some(parent);
    setup.allowed_branch = branch;
    setup.task_agent = true;
    // ADR 0030 O3: a task commits only to its own branch. A `shared` task has none, so it does
    // not mount `git_commit` (and the tool would refuse if it reached it); a worktree task gets
    // its `radian/<id>` and the tool is limited to it.
    setup.git_commit_requires_branch = true;
    // ADR 0030 O8: a task agent does not open memory (its single-writer lock).
    setup.memory_root = None;
    // ADR 0036 / ADR 0030 O11: layer 1 is the definition's body. The registry
    // resolved and snapshotted it at spawn, so the session never re-reads the file; without a
    // definition the built-in `task` role stands. `--system` (a fixed layer 1) does not apply.
    match agent {
        Some(agent) => {
            setup.role = Some(agent.name.clone());
            setup.system_prompt = Some(agent.body.clone());
        }
        None => {
            setup.role = Some("task".to_owned());
            setup.system_prompt = None;
        }
    }
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
            // ADR 0030 O11: a definition's `profile` may only narrow the task floor. `full` was
            // refused during resolution; `safe` narrows the builder's `workspace`. The profile
            // is rebuilt for `safe` (its write/shell rules are the tier's, and the host keeps
            // its own allow rules for `workspace`).
            if agent.and_then(|agent| agent.profile) == Some(Preset::Safe) {
                setup.tier = Preset::Safe;
                // A `safe` tier has no sandbox (ADR 0012/ADR 0016): the profile denies `shell`,
                // so nothing runs outside the host, and the worktree stays the coding root.
                setup.sandbox = None;
                setup.profile =
                    Profile::safe(workspace).map_err(|error| TaskError::new(error.to_string()))?;
            }
        }
    }
    // ADR 0030 problem 7 = A: the `tools` allow-list is enforced by denying every built-in it
    // does not name, so it can narrow a tier but never widen one (`deny` outranks `allow`).
    // ADR 0030 O11 ("not in the list is not mounted") also removes the unlisted built-ins from
    // the session's mounted specs and its `<runtime>` layer; the deny above stays beneath that
    // as defence in depth. MCP tools are never in this universe (the `mcp` list governs them).
    if let Some(agent) = agent
        && let Some(tools) = agent.tools.as_deref()
    {
        let denied: Vec<String> = builtin_tool_names()
            .into_iter()
            .filter(|name| !tools.iter().any(|allowed| allowed == name))
            .map(str::to_owned)
            .collect();
        setup.profile = setup.profile.clone().with_denied_tools(denied);
        setup.tools_allow = agent.tools.clone();
    }
    // ADR 0030 O11: a definition's `skills` list is the task's skill allow-list; `None` keeps
    // every discovered skill (the task default). A missing name is reported at spawn.
    if let Some(agent) = agent {
        setup.skills_allow = agent.skills.clone();
    }
    // ADR 0030 O8: a task has no MCP server unless its definition opts in by name (the default
    // stays none); an opted-in name with no configured server is dropped, never invented.
    if let Some(agent) = agent {
        setup
            .servers
            .retain(|server| agent.mcp.iter().any(|name| name == &server.id));
    }
    // ADR 0030 R1: the hard-deny list is added to whatever profile the builder gave,
    // and the hook's approval policy is `hook_approval` (auto only when nothing else can decide;
    // `auto` outranks nothing and never downgrades a `deny`).
    setup.profile = setup
        .profile
        .clone()
        .with_task_hard_deny()
        .with_approval(hook_approval);
    Ok(())
}

/// The built-in tool names a task can mount: the universe an agent definition's `tools`
/// allow-list narrows (ADR 0030 O11). MCP tools are dynamic (they depend on a server) and are
/// governed by the definition's `mcp` list, so they are not in it. A named agent's allow-list
/// narrows the same universe.
pub(crate) fn builtin_tool_names() -> Vec<&'static str> {
    vec![
        "read",
        "patch",
        "write",
        "list",
        "glob",
        "grep",
        "shell",
        "job_output",
        "job_list",
        "job_kill",
        "git_status",
        "git_diff",
        "memory_search",
        "memory_get",
        "memory_save",
        "memory_revise",
        "ask_user",
        "skill",
        "git_commit",
    ]
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

/// Resolves and snapshots the agent definition for one `spawn_task` call (ADR 0030 O11, revision
/// 2 item 4). The spawn arguments override the definition's fields (O4), and a profile that would
/// widen the task floor is refused here, before the worktree is created. `submit` calls this
/// **before** queueing, so a queued task starts from this snapshot and never re-reads the file.
fn resolve_task(request: &SpawnRequest) -> Result<ResolvedAgent, TaskError> {
    let resolved = resolve_agent(
        radian_home().as_deref(),
        &SpawnOverrides {
            agent: request.agent.as_deref(),
            isolation: request.isolation,
            model: request.model.as_deref(),
            profile: request.profile.as_deref(),
            approval: request.approval,
        },
    )
    .map_err(|error| TaskError::new(format!("cannot resolve the agent definition: {error}")))?;
    let isolation = resolved.isolation.unwrap_or_default();
    let approval = resolved.approval.unwrap_or(ApprovalPolicy::Auto);
    if !matches!(approval, ApprovalPolicy::Auto | ApprovalPolicy::OnAsk) {
        return Err(TaskError::new(
            "a task's approval is `auto` or `on-ask` (ADR 0030 R1)",
        ));
    }
    Ok(ResolvedAgent {
        agent: resolved,
        isolation,
        approval,
    })
}

/// The task worktree root for `workspace` (ADR 0039 decision 4), or `None` when the workspace
/// is not a git repository or this process has no usable `~/.radian`.
pub(crate) fn task_worktree_root(workspace: &Path) -> Option<WorktreeRoot> {
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
    let output = radian_coding::host_git::host_git_command(workspace)
        .ok()?
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

/// A host `git` command in the lead's workspace with the shared no-filter overrides (ADR 0039
/// decision 3). `git worktree add` checks files out (a smudge filter would run) and
/// `git worktree remove` reads the clean filter to test dirtiness, so both would otherwise
/// execute a driver reached through the writable `.gitattributes` or `$GIT_DIR/info/attributes`.
fn worktree_git(lead: &Path) -> Result<std::process::Command, TaskError> {
    radian_coding::host_git::host_git_command(lead).map_err(TaskError::new)
}

/// `git worktree add -b <branch> <worktree>` from the lead's workspace (host-side, ADR 0030
/// O3). The worktree root is opened one level at a time with `O_NOFOLLOW|O_DIRECTORY`
/// (ADR 0039 decision 4), so a symlink anywhere refuses before git runs.
pub fn create_worktree(
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
    let output = worktree_git(lead)?
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
    let mut level = home.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(TaskError::new(format!(
                "refusing the task worktree root {}: it is not a plain path",
                root.display()
            )));
        };
        level.push(name);
        let name = CString::new(name.as_encoded_bytes())
            .map_err(|_| TaskError::new("the worktree root path is not valid UTF-8"))?;
        let next = match open_dir_at(dir.as_fd(), &name) {
            Ok(next) => next,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                #[cfg(feature = "testing")]
                crate::task::testing::worktree_before_mkdir(&level);
                create_missing_level(dir.as_fd(), &name, root)?
            }
            Err(error) => return Err(root_error(root, &error)),
        };
        dir = next;
    }
    Ok(())
}

/// Creates a level of the worktree root that just opened as missing, then opens it through the
/// same `O_NOFOLLOW|O_DIRECTORY` open (ADR 0039 decision 4). Two task spawns in one process
/// share `RADIAN_HOME/worktrees`, so both can see the same level missing and both `mkdirat` it;
/// the loser's `EEXIST` means "someone else created it", not a failure. The open is what keeps
/// the refusal intact: a symlink planted at the name fails there and refuses the spawn.
#[cfg(unix)]
fn create_missing_level(
    parent: std::os::unix::io::BorrowedFd<'_>,
    name: &std::ffi::CStr,
    root: &Path,
) -> Result<std::os::unix::io::OwnedFd, TaskError> {
    match make_dir_at(parent, name) {
        Ok(()) => {}
        // Another spawn won the race for this level. Fall through to the open, which still
        // refuses anything that is not a plain directory.
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
        Err(error) => {
            return Err(TaskError::new(format!(
                "cannot create a level of {}: {error}",
                root.display()
            )));
        }
    }
    open_dir_at(parent, name).map_err(|error| root_error(root, &error))
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

/// `git worktree remove [--force] <worktree>` from the lead's workspace. Public so the
/// integration tests exercise the exact code the registry runs.
pub fn remove_worktree(lead: &Path, worktree: &Path, force: bool) -> Result<(), TaskError> {
    let path = worktree
        .to_str()
        .ok_or_else(|| TaskError::new("the worktree path is not valid UTF-8"))?;
    let mut command = worktree_git(lead)?;
    command.args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    command.arg(path);
    let output = command
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

/// The worktree's HEAD commit, or `None` when it is not a repository (or git fails). A host
/// `git` call through the shared helper.
fn git_head(worktree: &Path) -> Option<String> {
    let output = radian_coding::host_git::host_git_command(worktree)
        .ok()?
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!head.is_empty()).then_some(head)
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
    // it is also limited to its own branch. `require_branch = true`: a task with no branch of
    // its own (a `shared` task) never reaches here, but the tool refuses if it does.
    crate::git_commit::commit(
        worktree,
        pin,
        &message,
        &[],
        allowed_branch,
        protected,
        filters,
        true,
        true,
    )
    .map(Some)
}

/// The state of one task's end-of-task commit, and the `watch` the record waits on.
enum CommitState {
    /// No turn has ended; no commit is scheduled.
    Idle,
    /// The commit is running; `Done` arrives when it finishes.
    Running,
    /// The commit finished (or there was nothing to commit).
    Done(EndCommit),
}

/// The per-task end-of-task commit, invoked on a turn boundary. It needs no `LocalSession`
/// handle (that would be a cycle), only the worktree facts, the task's journal and the lead's.
/// Its result is awaitable (ADR 0039 decision 7).
struct TaskFinisher {
    id: String,
    lead_journal: Arc<SessionJournal>,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    protected: Vec<String>,
    filters: Vec<String>,
    pin: Option<RepoPin>,
    /// The task's journal, for the `GitCommitted` audit. Set right after `open_session`; the
    /// finisher only runs on a turn event, which is necessarily after that.
    journal: OnceLock<Arc<SessionJournal>>,
    runtime: tokio::runtime::Handle,
    /// The test gate at the start of the commit (ADR 0039 decision 7); `None` in production.
    gate: Option<CommitGate>,
    /// The host's notice callback, cloned from the registry (ADR 0030 O10).
    notifier: Option<TaskNotifier>,
    state: Mutex<CommitState>,
    /// `true` once the commit finished; the record's `await_end_commit` waits on it.
    done: watch::Sender<bool>,
    /// Whether the task's turn is running (ADR 0030 slice 6): set on `TurnStarted`, cleared on
    /// a turn boundary, so `wait_task`/`wait_any` can tell "still working" from "finished".
    running: AtomicBool,
    /// The last turn's outcome; `None` before the first turn.
    last_outcome: Mutex<Option<TurnOutcome>>,
    /// Bumped on every turn start and turn boundary, so a waiter wakes without polling.
    turns: watch::Sender<u64>,
    /// The FIFO pump wake (ADR 0030 O4/O8): a turn end frees a `max_tasks` slot.
    notify: Arc<Notify>,
    /// Set by the first end (a turn end or a removal), so the task writes exactly one
    /// `TaskFinished` (ADR 0030 O5): a second turn end or a later removal writes nothing.
    finished: AtomicBool,
}

impl TaskFinisher {
    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn last_outcome(&self) -> Option<TurnOutcome> {
        *self.last_outcome.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn subscribe_turns(&self) -> watch::Receiver<u64> {
        self.turns.subscribe()
    }

    /// Calls the host's notice callback, when one is installed (ADR 0030 O10).
    fn notify(&self, event: &RuntimeEvent) {
        if let Some(notifier) = &self.notifier {
            notifier(event);
        }
    }

    fn signal_turn(&self) {
        self.turns
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Fans one runtime event into the turn signal, then schedules the end-of-task commit on a
    /// turn boundary. The signal is bumped **after** the commit state is set to `Running`, so a
    /// waiter that wakes on it never sees the pre-commit `Idle`.
    fn on_turn_event(self: &Arc<Self>, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::TurnStarted { .. } => {
                self.running.store(true, Ordering::SeqCst);
                self.signal_turn();
            }
            RuntimeEvent::TurnFinished { outcome, .. } => self.on_turn_ended(*outcome),
            RuntimeEvent::TurnFailed { .. } => self.on_turn_ended(TurnOutcome::Failed),
            _ => {}
        }
    }

    /// A turn boundary: record the outcome, run the end-of-task commit, then wake the waiters
    /// and the FIFO pump (a `max_tasks` slot is free now, ADR 0030 O4/O8).
    fn on_turn_ended(self: &Arc<Self>, outcome: TurnOutcome) {
        self.running.store(false, Ordering::SeqCst);
        *self.last_outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        self.schedule_commit(outcome);
        self.signal_turn();
        self.notify.notify_one();
    }

    /// On the turn boundary that ends the task, run the end-of-task commit off the runtime
    /// thread and write the task's one `TaskFinished` (ADR 0030 O5). A worktree task's record
    /// follows its commit; a `shared` task has no commit, so its record is the turn end alone.
    /// A second turn end does nothing: the first end is the task's end (ADR 0030 O3, one task is
    /// one turn), and [`TaskFinisher::write_finished`] is once-only regardless.
    fn schedule_commit(self: &Arc<Self>, outcome: TurnOutcome) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(*state, CommitState::Idle) {
                return; // a second overlapping event does nothing
            }
            *state = CommitState::Running;
        }
        let worktree = self.worktree.clone();
        let pin = self.pin.clone();
        let id = self.id.clone();
        let branch = self.branch.clone();
        let protected = self.protected.clone();
        let filters = self.filters.clone();
        let journal = self.journal.get().cloned();
        let gate = self.gate.clone();
        let this = Arc::clone(self);
        self.runtime.spawn(async move {
            let end = match (worktree, pin) {
                (Some(worktree), Some(pin)) => {
                    let result = tokio::task::spawn_blocking(move || {
                        if let Some(gate) = &gate {
                            gate();
                        }
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
                    match result {
                        Ok(Ok(Some(committed))) => {
                            // Audit (best effort): the commit already happened; a write failure
                            // is not the caller's problem, exactly like `git_commit`'s own audit.
                            if let Some(journal) = &journal {
                                let _ = journal
                                    .append_runtime_event(RuntimeEvent::GitCommitted {
                                        commit: committed.commit.clone(),
                                        branch: committed.branch.clone(),
                                        files: committed.files,
                                    })
                                    .await;
                            }
                            EndCommit::Committed(committed)
                        }
                        Ok(Ok(None)) => EndCommit::None,
                        Ok(Err(failure)) => EndCommit::Failed(failure.message),
                        Err(error) => EndCommit::Failed(format!(
                            "the end-of-task commit task failed: {error}"
                        )),
                    }
                }
                // `shared` (ADR 0030 O3), or a worktree with no repository to commit in: no
                // end-of-task commit.
                _ => EndCommit::None,
            };
            // ADR 0030 O5 / ADR 0039 decision 7: the one `TaskFinished`, written after the
            // commit (and after the `GitCommitted` audit) so the record follows the commit. Its
            // `error` is the commit's failure, when the commit failed.
            let error = match &end {
                EndCommit::Failed(message) => Some(message.clone()),
                _ => None,
            };
            this.write_finished(outcome, error).await;
            *this.state.lock().unwrap_or_else(|e| e.into_inner()) = CommitState::Done(end);
            let _ = this.done.send(true);
        });
    }

    /// Writes this task's one `TaskFinished` to the lead's journal (ADR 0030 O5). The first
    /// caller wins; a second turn end or a later removal writes nothing. `error` is `Some` only
    /// for an abnormal end: a failed end-of-task commit here, or a spawn rolled back after its
    /// `TaskSpawned` (that path writes its own, before any turn can run).
    async fn write_finished(&self, outcome: TurnOutcome, error: Option<String>) {
        if self.finished.swap(true, Ordering::SeqCst) {
            return; // exactly one `TaskFinished` per task
        }
        let event = RuntimeEvent::TaskFinished {
            agent_session_id: self.id.clone(),
            outcome,
            error,
        };
        let _ = self.lead_journal.append_runtime_event(event.clone()).await;
        // ADR 0030 O10: tell the host's live view, so the TUI shows a `TaskFinished` notice (and
        // its task count drops) for every end, not only a failed end.
        self.notify(&event);
    }

    /// Resolves when the end-of-task commit has finished (ADR 0039 decision 7). `None` when no
    /// turn has ended, so no commit was ever scheduled.
    async fn await_end_commit(&self) -> Option<EndCommit> {
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            match &*state {
                CommitState::Idle => return None,
                CommitState::Done(end) => return Some(end.clone()),
                CommitState::Running => {}
            }
        }
        // `Done` is stored before the `watch` sends, so a receiver that arrives late still sees
        // it: `borrow_and_update` reads the current value, and `changed` is only awaited while
        // it is still `false`.
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match &*state {
            CommitState::Done(end) => Some(end.clone()),
            _ => None,
        }
    }
}

/// Wraps the host's `runtime_events` callback with the task finisher, so both see every event.
struct TaskRuntimeEvents {
    inner: Arc<dyn radian_runtime::RuntimeEvents>,
    finisher: Arc<TaskFinisher>,
    /// Bumped on every event so a lead's `wait_*` wakes on any task activity (ADR 0030 O4).
    wake: watch::Sender<u64>,
}

impl radian_runtime::RuntimeEvents for TaskRuntimeEvents {
    fn on_runtime_event(&self, event: &RuntimeEvent) {
        self.inner.on_runtime_event(event);
        self.finisher.on_turn_event(event);
        self.wake
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

/// Test-only seams for the task worktree root (feature `testing`, always on for this crate's own
/// tests through the self dev-dependency). A production build or a dependent crate never
/// installs one, so no hook fires outside a task test.
#[cfg(feature = "testing")]
pub mod testing {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type BeforeMkdir = Box<dyn Fn(&Path) + Send + Sync>;

    /// Armed levels of the worktree root: a hook runs the next time `vet_worktree_root` is about
    /// to create exactly that level — after the level opened as `ENOENT`, before the `mkdirat`.
    /// A test uses it to create the level first, which is the interleaving two concurrent spawns
    /// produce. Keyed by level, so tests running in parallel cannot consume each other's seam.
    static WORKTREE_BEFORE_MKDIR: Mutex<BTreeMap<PathBuf, BeforeMkdir>> =
        Mutex::new(BTreeMap::new());

    /// Arms the seam for `level`.
    pub fn arm_worktree_before_mkdir<F>(level: &Path, hook: F)
    where
        F: Fn(&Path) + Send + Sync + 'static,
    {
        WORKTREE_BEFORE_MKDIR
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(level.to_path_buf(), Box::new(hook));
    }

    pub(crate) fn worktree_before_mkdir(level: &Path) {
        let hook = WORKTREE_BEFORE_MKDIR
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(level);
        if let Some(hook) = hook {
            hook(level);
        }
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
