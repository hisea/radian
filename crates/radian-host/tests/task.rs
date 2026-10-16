//! ADR 0030 slice 4: the task-session lifecycle. A `TaskRegistry` opens a second session
//! in-process as a task agent: its own worktree on `radian/<id>`, the network off, no memory,
//! the `task` role, `parent.relation = "task"`, the lead's `TaskSpawned`, a task-branch
//! `git_commit`, and an end-of-task commit.
//!
//! Everything here is deterministic: temp repos, a mock model server only where a turn must
//! actually run, and a real local listener for the network case (skipped where no sandbox
//! backend or no python3 is available).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use agent_core::{NoopSink, ToolCall, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    CommitGate, HostBindings, Isolation, LocalSession, SessionError, SessionFacade, SessionId,
    SessionProjection, SessionQueue, SessionSetup, SpawnRequest, SpawnStage, TaskBuilder,
    TaskContext, TaskRegistry, TaskSeams, TaskSession, build_model, end_of_task_commit,
    open_session,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use radian_sandbox::{Sandbox, SandboxPolicy};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::{Value, json};

/// A unique scratch directory.
fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-task-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Canonical: `/var` -> `/private/var` on macOS, so git's own worktree paths match.
    std::fs::canonicalize(&dir).unwrap()
}

/// A private `RADIAN_HOME` for this test binary (ADR 0039 decision 4 resolves the task worktree
/// root through it). `RADIAN_HOME` is process-wide, so it is set once here and never changed;
/// the real `~/.radian` is never touched. Canonicalized like `scratch`.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-task-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, and every test in this binary calls
        // `isolated_home()` before it builds a registry, so no other thread is reading it.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

/// The Unix mode bits of `path` (`None` off Unix, where there are no such bits).
fn mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bits = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        Some(bits)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Records the task sessions whose driver ended (their `driver_stopped` binding fired), so a
/// test can assert a failed spawn left no driver running.
#[derive(Default)]
struct DriverStops {
    stopped: Mutex<Vec<String>>,
}

impl DriverStops {
    fn record(&self, session_id: &str) {
        self.stopped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(session_id.to_owned());
    }

    fn count(&self, session_id: &str) -> usize {
        self.stopped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|id| *id == session_id)
            .count()
    }
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("git runs");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A `workspace` sandbox for `ws` (network on by default), or `None` when this machine has no
/// working backend.
fn workspace_sandbox(ws: &Path) -> Option<Sandbox> {
    let policy = SandboxPolicy::workspace_write(ws).ok()?;
    Sandbox::from_policy(policy).ok()
}

fn model(
    session_id: &str,
    base_url: &str,
    slot: Arc<radian_runtime::ConfigSlot>,
    route: Option<radian_host::ModelRoute>,
) -> radian_host::SwitchableModel {
    let model = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: base_url.to_owned(),
        model: "mock".into(),
        api_key: Some("sk-test".into()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
    })
    .unwrap();
    // ADR 0030 O11: a definition's `model` becomes the session's initial route. A real host
    // resolves credentials from that route; the test's mock server answers either way.
    let initial =
        route.unwrap_or_else(|| radian_host::ModelRoute::new("openai-compatible", "mock"));
    radian_host::SwitchableModel::new(
        model,
        initial,
        slot,
        radian_host::RouteContext {
            providers: std::collections::BTreeMap::new(),
            session_id: session_id.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    )
}

/// The host-specific builder a test gives the registry: a mock model and, for a worktree task,
/// a `workspace` sandbox built with the network **on** (the registry must turn it off).
fn builder(session_dir: PathBuf, base_url: String, stops: Arc<DriverStops>) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            // ADR 0030 O11: the definition's route is the session's route. A real host builds the
            // provider from it; the test's mock model answers either way, and the slot carries
            // the route so `model_route()` reflects it.
            let route = ctx.agent.and_then(|agent| agent.model.clone());
            let slot = radian_runtime::ConfigSlot::new(
                route
                    .as_ref()
                    .map(radian_host::ModelRoute::to_value)
                    .unwrap_or(Value::Null),
            );
            let stopped = Arc::clone(&stops);
            let session_id = ctx.session_id.to_owned();
            let (sandbox, tier, profile) = match ctx.isolation {
                Isolation::Shared => (
                    None,
                    Preset::Safe,
                    Profile::safe(&workspace).map_err(|e| fail(e.to_string()))?,
                ),
                Isolation::Worktree => {
                    let sandbox = workspace_sandbox(&workspace)
                        .ok_or_else(|| fail("no sandbox backend here".into()))?;
                    (
                        Some(sandbox),
                        Preset::WorkspaceWrite,
                        // ADR 0030 R1: a task agent runs `auto` (slice 5 adds the hard-deny list).
                        Profile::workspace_write(&workspace)
                            .map_err(|e| fail(e.to_string()))?
                            .with_approval(ApprovalPolicy::Auto),
                    )
                }
            };
            Ok(TaskSession {
                setup: SessionSetup {
                    session_id: SessionId::new(ctx.session_id),
                    session_dir: session_dir.clone(),
                    workspace: workspace.clone(),
                    coding_root: Some(workspace.clone()),
                    model: model(ctx.session_id, &base_url, Arc::clone(&slot), route),
                    config_slot: slot,
                    config_queue: radian_runtime::ConfigQueue::new(),
                    budget_for: None,
                    price_for: None,
                    protected_branches: Vec::new(),
                    allowed_filters: None,
                    shell_auto_background: None,
                    servers: Vec::new(),
                    role: None,
                    system_prompt: None,
                    context_budget: None,
                    auto_compact: false,
                    compaction_settings: Value::Null,
                    compaction_settings_for: None,
                    retries: 0,
                    sandbox,
                    tier,
                    profile,
                    access_builder: None,
                    // Deliberately the wrong value: the registry must set it.
                    task_agent: false,
                    parent: None,
                    allowed_branch: None,
                    memory_root: Some(session_dir.clone()),
                    skills_extra_paths: Vec::new(),
                    skills_builtin: true,
                    skills_disabled: Vec::new(),
                    tools_allow: None,
                    skills_allow: None,
                    memory_injection: radian_host::memory::InjectionConfig::default(),
                    trust: radian_host::trust::Setup::untrusted(&workspace, false),
                    resume: false,
                    config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
                    turn_options: TurnOptions::default(),
                    turn_timeout: None,
                    wording: radian_host::CLI_WORDING,
                    capabilities: radian_host::SessionCapability::ALL.to_vec(),
                },
                bindings: HostBindings {
                    approvals: radian_host::ApprovalRouting::Queued,
                    shared_queue: None,
                    ask_user: false,
                    audit: Arc::new(|_journal| {
                        Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                    }),
                    events: Arc::new(NoopSink),
                    projection: Arc::new(SessionProjection::new()),
                    runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
                    driver_stopped: Arc::new(move |_| stopped.record(&session_id)),
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                    task_builder: None,
                },
            })
        },
    )
}

/// The pieces a test keeps: the lead's dir, its journal, its queue, and the registry.
struct Fixture {
    scratch: PathBuf,
    home: PathBuf,
    lead: PathBuf,
    session_dir: PathBuf,
    queue: Arc<SessionQueue>,
    stops: Arc<DriverStops>,
    registry: TaskRegistry,
}

impl Fixture {
    fn new(label: &str, base_url: &str) -> Self {
        Self::with_seams(label, base_url, TaskSeams::default())
    }

    fn with_seams(label: &str, base_url: &str, seams: TaskSeams) -> Self {
        let home = isolated_home();
        let scratch = scratch(label);
        let lead = scratch.join("lead");
        std::fs::create_dir_all(&lead).unwrap();
        assert!(git(&lead, &["init", "-q", "-b", "main"]));
        assert!(git(
            &lead,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ]
        ));
        let session_dir = scratch.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let lead_journal = Arc::new(
            SessionJournal::create(
                session_dir.join("lead.jsonl"),
                "lead",
                SessionCreatedData::new(lead.to_string_lossy(), json!({})),
            )
            .expect("the lead journal"),
        );
        let queue: Arc<SessionQueue> = Arc::new(SessionQueue::new());
        let stops = Arc::new(DriverStops::default());
        let registry = TaskRegistry::new(
            lead.clone(),
            "lead",
            lead_journal,
            Arc::clone(&queue),
            builder(session_dir.clone(), base_url.to_owned(), Arc::clone(&stops)),
        )
        .with_seams(seams);
        Self {
            scratch,
            home,
            lead,
            session_dir,
            queue,
            stops,
            registry,
        }
    }

    /// The directory task worktrees live under (ADR 0039 decision 4).
    fn worktree_root(&self) -> PathBuf {
        self.registry
            .worktree_root()
            .expect("the lead is a git repository")
            .to_path_buf()
    }

    fn task_journal(&self, id: &str) -> Value {
        let text = std::fs::read_to_string(self.session_dir.join(format!("{id}.jsonl"))).unwrap();
        text.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| value["event"] == "SessionCreated")
            .expect("a SessionCreated line")
    }

    fn lead_journal_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.session_dir.join("lead.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

/// A worktree task opens with a clean worktree on `radian/<id>`; its journal records the lead
/// as a `task` parent; the lead's journal records `TaskSpawned`.
#[tokio::test]
async fn a_task_opens_in_a_worktree_on_its_branch_with_a_task_parent() {
    let fixture = Fixture::new("worktree", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return; // no sandbox backend here
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");

    // It is a git worktree of the lead repo, on its own branch.
    let worktree = record.worktree.clone().expect("a worktree");
    assert_eq!(worktree, fixture.worktree_root().join("t1"));
    assert_eq!(record.branch.as_deref(), Some("radian/t1"));
    let list = git_out(&fixture.lead, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains(&format!("worktree {}", worktree.display())),
        "git worktree list:\n{list}"
    );
    assert_eq!(
        git_out(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "radian/t1"
    );
    assert_eq!(
        git_out(&worktree, &["status", "--porcelain"]),
        "",
        "clean worktree"
    );

    // The task journal's parent is the lead, relation `task`, workspace the worktree.
    let created = fixture.task_journal("t1");
    assert_eq!(created["data"]["parent"]["relation"], "task");
    assert_eq!(created["data"]["parent"]["session_id"], "lead");
    assert_eq!(
        created["data"]["parent"]["seq"].as_u64(),
        Some(record.start_seq)
    );
    assert_eq!(
        created["data"]["workspace"].as_str(),
        Some(worktree.to_string_lossy().as_ref())
    );

    // The lead's journal records the spawn.
    let lines = fixture.lead_journal_lines();
    let spawned: Vec<&Value> = lines
        .iter()
        .filter(|v| v["event"] == "TaskSpawned")
        .collect();
    assert_eq!(spawned.len(), 1, "one TaskSpawned");
    assert_eq!(spawned[0]["data"]["agent_session_id"], "t1");
    assert_eq!(
        spawned[0]["data"]["worktree"].as_str(),
        Some(worktree.to_string_lossy().as_ref())
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// The task is a task agent (`/mode` refused) with no memory, and — ADR 0030 — it
/// does **not** share the lead's human queue: its asks and questions go to the task-side pending
/// queue, and only an escalation reaches the human.
#[tokio::test]
async fn a_task_is_a_task_agent_with_no_memory_and_no_human_queue() {
    let fixture = Fixture::new("shared-queue", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");

    assert!(
        !Arc::ptr_eq(record.session.queue(), &fixture.queue),
        "a task must not route its asks to the human queue (ADR 0030)"
    );
    assert!(
        !record.session.facts().memory_enabled,
        "a task agent opens no memory (ADR 0030 O8)"
    );
    let refused = record
        .session
        .set_mode(radian_host::Mode::Lead)
        .expect_err("a task agent cannot switch the mode");
    assert!(refused.reason.contains("task agent"), "{}", refused.reason);

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// The installed sandbox keeps the network off (ADR 0030 item 2): an outbound
/// connect that succeeds for the host fails inside it.
#[tokio::test]
async fn the_task_sandbox_turns_the_network_off() {
    let fixture = Fixture::new("network", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    if Command::new("python3").arg("--version").output().is_err() {
        return; // no python3 to run the connect
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");

    let policy = record
        .sandbox_policy
        .clone()
        .expect("a worktree task has a sandbox policy");
    assert!(
        !policy.network,
        "the task sandbox must have the network off"
    );
    assert_eq!(
        policy.git_access,
        radian_sandbox::GitAccess::ReadOnly,
        "a task agent keeps .git read-only (ADR 0038 decision 5)"
    );
    assert_eq!(policy.workspace, record.worktree.clone().unwrap());

    // A real outbound connect: the host reaches the listener; the task sandbox does not.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "import socket,sys; s=socket.create_connection((\"127.0.0.1\",{port}),2); s.close(); sys.exit(0)"
    );
    let control = Command::new("python3")
        .args(["-c", &script])
        .status()
        .expect("python3");
    assert!(control.success(), "the control connect must succeed");

    let sandbox = Sandbox::from_policy(policy).expect("a sandbox");
    let mut command = sandbox
        .command("python3", ["-c", &script])
        .expect("a command");
    let out = command.output().expect("the sandboxed command runs");
    assert!(
        !out.status.success(),
        "a task sandbox must not reach the network: {out:?}"
    );
    drop(listener);

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// `shared` isolation: no worktree, no branch, the lead's workspace, forced `safe`.
#[tokio::test]
async fn shared_isolation_is_the_lead_workspace_and_safe() {
    let fixture = Fixture::new("shared", "http://127.0.0.1:1/v1");
    let mut request = SpawnRequest::new("t1");
    request.isolation = Some(Isolation::Shared);
    let record = fixture.registry.spawn(request).await.expect("spawn");

    assert_eq!(record.isolation, Isolation::Shared);
    assert!(record.worktree.is_none(), "shared has no worktree");
    assert!(record.branch.is_none(), "shared has no branch");
    assert!(record.sandbox_policy.is_none(), "shared is the safe tier");
    assert_eq!(record.session.workspace(), fixture.lead.as_path());
    assert_eq!(
        record.session.sandbox_mode(),
        None,
        "shared is safe: no sandbox, no shell"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A sandboxed `git commit` in the task worktree fails (ADR 0030 O3: `.git` is read-only), so a
/// task's commits go through the host's `git_commit`. Skips without a backend.
#[tokio::test]
async fn a_shell_git_commit_fails_in_the_task_worktree() {
    let fixture = Fixture::new("shell-commit", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    std::fs::write(worktree.join("change.txt"), "x\n").unwrap();
    let policy = record.sandbox_policy.clone().unwrap();
    let sandbox = Sandbox::from_policy(policy).expect("a sandbox");

    let out = radian_coding::run_user_bash(
        &worktree,
        &sandbox,
        "git add -A && git commit -m from-the-worktree",
        &agent_core::CancellationToken::new(),
    )
    .await
    .expect("the sandboxed command ran");
    assert_ne!(
        out.exit_code,
        Some(0),
        "the task worktree's .git is read-only: {}",
        out.stderr
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A `shared` task writes no end-of-task commit: even with the lead's workspace dirty, its
/// turn end must not commit the user's changes (ADR 0030 O3: no worktree, no end commit).
#[tokio::test]
async fn a_shared_task_writes_no_end_commit() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "done"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let fixture = Fixture::new("shared-end", &server.base_url);
    // A feature branch, so an erroneous end commit on the lead workspace *could* succeed and
    // the test can tell it apart from the correct "no commit".
    assert!(git(
        &fixture.lead,
        &["checkout", "-q", "-b", "lead/feature"]
    ));
    // The user's own uncommitted change in the lead workspace.
    std::fs::write(fixture.lead.join("user-work.txt"), "the user's change\n").unwrap();

    let mut request = SpawnRequest::new("t1");
    request.isolation = Some(Isolation::Shared);
    request.brief = Some("look around".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let journal = fixture.session_dir.join("t1.jsonl");
    loop {
        if std::fs::read_to_string(&journal).is_ok_and(|text| text.contains("\"TurnFinished\"")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The turn ended; give a wrong finisher a bounded window to commit the lead workspace. A
    // correct run makes no commit, so the assertions below hold after the window.
    let quiet = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let committed = git_out(&fixture.lead, &["log", "-1", "--pretty=%s"]) != "init"
            || git_out(&fixture.lead, &["status", "--porcelain"]).is_empty();
        if committed || std::time::Instant::now() >= quiet {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(
        git_out(&fixture.lead, &["log", "-1", "--pretty=%s"]),
        "init",
        "a shared task must not commit the user's workspace"
    );
    assert_ne!(
        git_out(&fixture.lead, &["status", "--porcelain"]),
        "",
        "the user's change stays uncommitted"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
    let _ = record;
}

/// The end-of-task commit commits the worktree's changes with the fixed message; a second call
/// with no changes makes no commit.
#[tokio::test]
async fn the_end_of_task_commit_cleans_the_worktree() {
    let fixture = Fixture::new("end", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    std::fs::write(worktree.join("made.txt"), "by the task\n").unwrap();

    let pin = radian_host::git_commit::RepoPin::resolve(&worktree).expect("a repo");
    let committed = end_of_task_commit(
        "t1",
        &radian_session::TurnOutcome::Completed,
        &worktree,
        &pin,
        Some("radian/t1"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .expect("the end commit")
    .expect("there were changes");
    assert_eq!(committed.branch, "radian/t1");
    assert_eq!(
        git_out(&worktree, &["log", "-1", "--pretty=%s"]),
        "radian: end of task t1 (completed)"
    );
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");

    // Nothing left to commit: no second commit.
    let again = end_of_task_commit(
        "t1",
        &radian_session::TurnOutcome::Completed,
        &worktree,
        &pin,
        Some("radian/t1"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .expect("no failure with a clean tree");
    assert!(again.is_none(), "a clean worktree makes no commit");

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// With no changes at all, the end-of-task commit makes no commit and HEAD is unchanged.
#[tokio::test]
async fn the_end_of_task_commit_makes_no_commit_without_changes() {
    let fixture = Fixture::new("end-none", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    let before = git_out(&worktree, &["rev-parse", "HEAD"]);

    let pin = radian_host::git_commit::RepoPin::resolve(&worktree).expect("a repo");
    let committed = end_of_task_commit(
        "t1",
        &radian_session::TurnOutcome::Cancelled,
        &worktree,
        &pin,
        Some("radian/t1"),
        &[],
        &radian_host::git_commit::default_allowed_filters(),
    )
    .expect("no failure with a clean tree");
    assert!(committed.is_none(), "no changes, no commit");
    assert_eq!(git_out(&worktree, &["rev-parse", "HEAD"]), before);

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A task's `git_commit` is limited to `radian/<id>` (ADR 0030 O3): on another branch the host
/// tool refuses, so the worktree stays on that branch and uncommitted. This is what pins the
/// `with_allowed_branch` wiring in `open_session` (not only `commit()`'s own check).
#[tokio::test]
async fn a_task_git_commit_is_limited_to_its_branch() {
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "git_commit", "arguments": "{\"message\":\"sneaky\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "done"}), None),
            chunk(json!({}), Some("stop")),
        ]),
    ])
    .await;
    let fixture = Fixture::new("branch", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    // Spawn with no brief, so the worktree can be put on another branch before the turn.
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    std::fs::write(worktree.join("something.txt"), "a change to commit\n").unwrap();
    assert!(git(&worktree, &["checkout", "-q", "-b", "other"]));

    // Now drive the turn: the model calls `git_commit`, which must refuse (wrong branch).
    record
        .session
        .inbox()
        .followup(
            agent_core::Message::user("commit the change"),
            agent_core::InputSource::Subagent,
        )
        .await
        .expect("the brief");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.requests().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(
        git_out(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "other"
    );
    assert_eq!(
        git_out(&worktree, &["log", "-1", "--pretty=%s"]),
        "init",
        "the commit must be refused on a branch that is not radian/t1"
    );
    assert_ne!(
        git_out(&worktree, &["status", "--porcelain"]),
        "",
        "the change stays uncommitted"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// Opens a plain (non-registry) session on `repo` with the host's `git_commit`, so a test can
/// pin the `refuse_main` wiring in `open_session` (ADR 0038 decision 4).
async fn open_plain(
    repo: &Path,
    session_dir: &Path,
    base_url: &str,
    task_agent: bool,
) -> Arc<LocalSession> {
    let slot = radian_runtime::ConfigSlot::new(Value::Null);
    let session_id = if task_agent { "task" } else { "general" };
    let model = model(session_id, base_url, Arc::clone(&slot), None);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(session_id),
            session_dir: session_dir.to_path_buf(),
            workspace: repo.to_path_buf(),
            coding_root: Some(repo.to_path_buf()),
            model,
            config_slot: slot,
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            protected_branches: Vec::new(),
            allowed_filters: None,
            shell_auto_background: None,
            servers: Vec::new(),
            role: None,
            system_prompt: None,
            context_budget: None,
            auto_compact: false,
            compaction_settings: Value::Null,
            compaction_settings_for: None,
            retries: 0,
            // No sandbox: this test is about the host-side `git_commit`, which runs outside it.
            sandbox: None,
            tier: Preset::WorkspaceWrite,
            profile: Profile::workspace_write(repo)
                .unwrap()
                .with_approval(ApprovalPolicy::Auto),
            access_builder: None,
            task_agent,
            parent: None,
            allowed_branch: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            tools_allow: None,
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            trust: radian_host::trust::Setup::untrusted(repo, false),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Queued,
            shared_queue: None,
            ask_user: false,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            memory_notice_line: Arc::new(|_line: &str| {}),
            task_builder: None,
        },
    )
    .await
    .expect("open");
    Arc::new(session)
}

/// A mock server whose model calls `git_commit { message: "session-made" }`, then answers.
async fn commit_server() -> MockServer {
    MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "git_commit", "arguments": "{\"message\":\"session-made\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "done"}), None),
            chunk(json!({}), Some("stop")),
        ]),
    ])
    .await
}

/// Drive one turn and wait for it to finish (two model requests: the tool call and the answer).
async fn drive_turn(
    session: &Arc<LocalSession>,
    server: &MockServer,
    source: agent_core::InputSource,
) {
    session
        .inbox()
        .followup(agent_core::Message::user("commit it"), source)
        .await
        .expect("the input");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.requests().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A general session's `git_commit` may commit on `main` (ADR 0038 decision 4): the wiring in
/// `open_session` passes `refuse_main = false` for a non-task agent.
#[tokio::test]
async fn a_general_session_commits_on_main_through_the_session() {
    let dir = scratch("general-main");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(git(&repo, &["init", "-q", "-b", "main"]));
    assert!(git(
        &repo,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]
    ));
    std::fs::write(repo.join("change.txt"), "x\n").unwrap();
    let server = commit_server().await;
    let session = open_plain(&repo, &dir.join("sessions"), &server.base_url, false).await;

    drive_turn(&session, &server, agent_core::InputSource::User).await;
    assert_eq!(
        git_out(&repo, &["log", "-1", "--pretty=%s"]),
        "session-made",
        "a general session may commit on main"
    );
    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A task agent's `git_commit` refuses `main` even with no branch restriction (ADR 0038
/// decision 4): the wiring passes `refuse_main = true` for a task agent.
#[tokio::test]
async fn a_task_agent_session_refuses_a_commit_on_main() {
    let dir = scratch("task-main");
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    assert!(git(&repo, &["init", "-q", "-b", "main"]));
    assert!(git(
        &repo,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]
    ));
    std::fs::write(repo.join("change.txt"), "x\n").unwrap();
    let server = commit_server().await;
    let session = open_plain(&repo, &dir.join("sessions"), &server.base_url, true).await;

    drive_turn(&session, &server, agent_core::InputSource::Subagent).await;
    assert_eq!(
        git_out(&repo, &["log", "-1", "--pretty=%s"]),
        "init",
        "a task agent may not commit on main"
    );
    assert_ne!(git_out(&repo, &["status", "--porcelain"]), "");
    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// `remove_task` refuses a dirty worktree and `force` discards it; a clean worktree is removed.
#[tokio::test]
async fn remove_refuses_a_dirty_worktree_and_force_discards_it() {
    let fixture = Fixture::new("remove", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    std::fs::write(worktree.join("uncommitted.txt"), "oops\n").unwrap();

    let refused = fixture
        .registry
        .remove("t1", false)
        .await
        .expect_err("a dirty worktree is refused");
    assert!(refused.to_string().contains("uncommitted"), "{refused}");
    assert!(fixture.registry.get("t1").is_some(), "the task is kept");
    assert!(worktree.exists(), "the worktree is kept");

    fixture
        .registry
        .remove("t1", true)
        .await
        .expect("force remove");
    assert!(!worktree.exists(), "force removes the worktree");
    assert!(fixture.registry.get("t1").is_none());
    let list = git_out(&fixture.lead, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("t1"),
        "the worktree is unregistered:\n{list}"
    );
}

/// On the turn's end the runtime commits the worktree (ADR 0030 item 1 B): a model
/// that writes a file leaves the worktree clean at its own end-of-task commit.
#[tokio::test]
async fn the_turn_end_commits_the_worktree() {
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "write", "arguments": "{\"path\":\"made.txt\",\"content\":\"by the task\\n\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "done"}), None),
            chunk(json!({}), Some("stop")),
        ]),
    ])
    .await;
    let fixture = Fixture::new("turn", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("write made.txt".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    let worktree = record.worktree.clone().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let subject = git_out(&worktree, &["log", "-1", "--pretty=%s"]);
        if subject == "radian: end of task t1 (completed)" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the end-of-task commit never landed: subject {subject:?}, status {:?}",
            git_out(&worktree, &["status", "--porcelain"])
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(git_out(&worktree, &["status", "--porcelain"]), "");
    assert!(worktree.join("made.txt").exists());

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// ADR 0039 decision 3 (and ADR 0030): `git worktree add` checks files out, so a
/// smudge filter named in the writable `.gitattributes` would run in the host; `git worktree
/// remove` reads the clean filter to test dirtiness. Neither may run a repository filter command.
#[test]
fn creating_and_removing_a_worktree_runs_no_filter() {
    let lead = scratch("wt-filter-lead");
    assert!(git(&lead, &["init", "-q", "-b", "main"]));
    assert!(git(&lead, &["config", "user.email", "t@example.com"]));
    assert!(git(&lead, &["config", "user.name", "t"]));
    // Commit the tree (with an attribute that will name a filter) while no filter exists yet.
    std::fs::write(lead.join("a.txt"), "hello\n").unwrap();
    std::fs::write(lead.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    assert!(git(&lead, &["add", "-A"]));
    assert!(git(&lead, &["commit", "-q", "-m", "init"]));

    // Now the filter exists in the repo config, and its command touches a marker.
    let marker = scratch("wt-filter-marker").join("FILTER-RAN");
    let script = lead.join("evil.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(git(
        &lead,
        &["config", "filter.evil.clean", script.to_str().unwrap()]
    ));
    assert!(git(
        &lead,
        &["config", "filter.evil.smudge", script.to_str().unwrap()]
    ));
    assert!(git(&lead, &["config", "filter.evil.required", "true"]));

    // ADR 0039 decision 4: the worktree lives below the radian home, which create_worktree vets.
    let home = scratch("wt-filter-home");
    let wt = home.join("wt");
    radian_host::task::create_worktree(&lead, &home, &wt, "radian/wt-filter")
        .expect("worktree add");
    assert!(
        wt.join("a.txt").exists(),
        "the worktree must be checked out"
    );
    assert!(
        !marker.exists(),
        "worktree add must not run a smudge filter in the host"
    );

    radian_host::task::remove_worktree(&lead, &wt, false).expect("worktree remove");
    assert!(
        !marker.exists(),
        "worktree remove must not run a filter in the host"
    );
}

/// ADR 0039 decision 4: two task spawns in one process share `RADIAN_HOME/worktrees`, so both can
/// see the same level of the root missing and both `mkdirat` it. The loser's `EEXIST` means
/// "someone else created it", not a failure: the worktree is still created, through the same
/// `O_NOFOLLOW|O_DIRECTORY` open. The seam creates the level between the miss and the `mkdirat`,
/// which is the exact interleaving; a second caller's `mkdirat` is what produces the `EEXIST`.
#[test]
fn a_worktree_level_created_by_a_concurrent_spawn_still_succeeds() {
    let lead = scratch("root-race-lead");
    assert!(git(&lead, &["init", "-q", "-b", "main"]));
    assert!(git(&lead, &["config", "user.email", "t@example.com"]));
    assert!(git(&lead, &["config", "user.name", "t"]));
    std::fs::write(lead.join("a.txt"), "hello\n").unwrap();
    assert!(git(&lead, &["add", "-A"]));
    assert!(git(&lead, &["commit", "-q", "-m", "init"]));

    let home = scratch("root-race-home");
    let root = home.join("worktrees").join("key");
    let wt = root.join("t1");

    radian_host::task::testing::arm_worktree_before_mkdir(&root, |level| {
        std::fs::create_dir_all(level).expect("the concurrent creator");
    });
    radian_host::task::create_worktree(&lead, &home, &wt, "radian/t1")
        .expect("a level created by a concurrent spawn must not fail the worktree add");
    assert!(
        wt.join("a.txt").exists(),
        "the worktree must be checked out"
    );
    radian_host::task::remove_worktree(&lead, &wt, false).expect("worktree remove");
}

/// The `EEXIST` path still refuses a symlink left at the level by the concurrent creator: the
/// open after the `mkdirat` is the same `O_NOFOLLOW|O_DIRECTORY` open (ADR 0039 decision 4), so a
/// lost race cannot become a hole.
#[cfg(unix)]
#[test]
fn a_symlink_left_by_a_concurrent_spawn_is_still_refused() {
    let lead = scratch("root-race-link-lead");
    assert!(git(&lead, &["init", "-q", "-b", "main"]));
    assert!(git(&lead, &["config", "user.email", "t@example.com"]));
    assert!(git(&lead, &["config", "user.name", "t"]));
    std::fs::write(lead.join("a.txt"), "hello\n").unwrap();
    assert!(git(&lead, &["add", "-A"]));
    assert!(git(&lead, &["commit", "-q", "-m", "init"]));

    let home = scratch("root-race-link-home");
    let root = home.join("worktrees").join("key");
    let wt = root.join("t1");
    let outside = scratch("root-race-link-outside");

    radian_host::task::testing::arm_worktree_before_mkdir(&root, {
        let outside = outside.clone();
        move |level| {
            std::os::unix::fs::symlink(&outside, level).expect("the concurrent creator's symlink");
        }
    });
    let error = radian_host::task::create_worktree(&lead, &home, &wt, "radian/t1")
        .expect_err("a symlink at the level must be refused");
    assert!(
        error.to_string().contains("symlink"),
        "the error must name the symlink: {error}"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "the symlink target stays empty"
    );
    assert!(!wt.exists(), "nothing may be written through the symlink");
}

// ---------------------------------------------------------------------------------------------
// ADR 0039 decision 4: the task worktree root is outside the workspace.
// ---------------------------------------------------------------------------------------------

/// A task worktree lives under `RADIAN_HOME/worktrees/<repo-key>/`, not inside the lead's
/// workspace, and the root levels are owner-only (ADR 0039 decision 4).
#[tokio::test]
async fn the_task_worktree_is_outside_the_workspace_on_a_per_repository_root() {
    let fixture = Fixture::new("root", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return; // no sandbox backend here
    }
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().expect("a worktree");

    let root = fixture.worktree_root();
    assert_eq!(worktree, root.join("t1"));
    assert!(
        root.starts_with(fixture.home.join("worktrees")),
        "the root must be under RADIAN_HOME/worktrees: {}",
        root.display()
    );
    assert!(
        !worktree.starts_with(&fixture.lead),
        "the worktree must be outside the workspace: {}",
        worktree.display()
    );
    // `<repo-key>` names the repository's directory plus a short hash.
    let key = root.file_name().unwrap().to_str().unwrap();
    assert!(key.starts_with("lead-"), "{key}");
    // The old in-workspace root is not used at all.
    assert!(!fixture.lead.join(".radian-worktrees").exists());
    // Every level the host creates is owner-only (0700).
    if let Some(bits) = mode(&root) {
        assert_eq!(bits, 0o700, "{}", root.display());
        assert_eq!(mode(root.parent().unwrap()), Some(0o700));
    }

    // Git itself knows the worktree, so the task's `git_commit` works from the new location.
    assert_eq!(
        git_out(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "radian/t1"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// The model swaps the old in-workspace `.radian-worktrees` for a symlink pointing outside the
/// workspace: nothing follows it, because the root is elsewhere now (ADR 0039 decision 4).
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_at_the_old_worktree_root_changes_nothing() {
    let fixture = Fixture::new("old-symlink", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let outside = fixture.scratch.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.lead.join(".radian-worktrees"))
        .expect("the swapped old root");

    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");

    assert_eq!(
        record.worktree.clone().unwrap(),
        fixture.worktree_root().join("t1")
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "nothing may be written through the old root's symlink"
    );
    assert!(
        fixture.worktree_root().join("t1").exists(),
        "the worktree is at the new root"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
}

/// A symlink at the `<repo-key>` level of the new root is refused before git runs (ADR 0039
/// decision 4). The `worktrees/` level itself is per-process, so it is covered by
/// `tests/task_root.rs`, which owns its own `RADIAN_HOME`.
#[cfg(unix)]
#[tokio::test]
async fn a_symlink_under_the_new_root_is_refused() {
    let fixture = Fixture::new("new-symlink", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let root = fixture.worktree_root();
    let outside = fixture.scratch.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // `<home>/worktrees/<repo-key>` is a symlink pointing outside the radian home.
    // `worktrees/` is shared by every test in this binary (one RADIAN_HOME per process): create
    // it owner-only like the host does, or a test that checks the level's 0700 mode would see
    // this test's 0755 when it runs first.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root.parent().unwrap())
            .unwrap();
    }
    std::os::unix::fs::symlink(&outside, &root).expect("the swapped root");

    let error = match fixture.registry.spawn(SpawnRequest::new("t1")).await {
        Ok(_) => panic!("a symlink at a root level must be refused"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("symlink"),
        "the error must name the symlink: {error}"
    );
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "the symlink target stays empty"
    );
    assert!(fixture.registry.get("t1").is_none());
    // Only a symlink check that runs **before** git can keep git from writing through it.
    let list = git_out(&fixture.lead, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("t1"),
        "git must not know a worktree:\n{list}"
    );
}

// ---------------------------------------------------------------------------------------------
// ADR 0039 decision 5: spawning is one transaction.
// ---------------------------------------------------------------------------------------------

/// A failure at any of the four spawn points leaves no worktree, no driver and a consistent
/// registry, and pairs every `TaskSpawned` with a `TaskFinished` (ADR 0039 decision 5).
#[tokio::test]
async fn a_failed_spawn_leaves_nothing_behind_and_pairs_a_task_finished() {
    for stage in [
        SpawnStage::TaskSpawned,
        SpawnStage::Builder,
        SpawnStage::Enforce,
        SpawnStage::Brief,
    ] {
        a_failed_spawn_at(stage).await;
    }
}

async fn a_failed_spawn_at(stage: SpawnStage) {
    let seams = TaskSeams {
        spawn: Some(Arc::new(move |at: SpawnStage| {
            if at == stage {
                Err(format!("test seam: aborting at {at:?}"))
            } else {
                Ok(())
            }
        })),
        commit_gate: None,
    };
    let fixture = Fixture::with_seams("fail", "http://127.0.0.1:1/v1", seams);
    if workspace_sandbox(&fixture.lead).is_none() {
        return; // no sandbox backend here
    }
    let root = fixture.worktree_root();
    let mut request = SpawnRequest::new("t1");
    if stage == SpawnStage::Brief {
        // The `Brief` seam only exists when there is a brief to send.
        request.brief = Some("go".to_owned());
    }
    let error = match fixture.registry.spawn(request).await {
        Ok(_) => panic!("{stage:?}: the seam must abort the spawn"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("test seam"),
        "{stage:?}: {error}"
    );
    // No worktree, in the filesystem or in git.
    assert!(!root.join("t1").exists(), "{stage:?}: a worktree is left");
    let list = git_out(&fixture.lead, &["worktree", "list", "--porcelain"]);
    assert!(
        !list.contains("t1"),
        "{stage:?}: git knows a worktree:\n{list}"
    );
    // A consistent registry: no entry, no id.
    assert!(fixture.registry.get("t1").is_none(), "{stage:?}");
    assert!(fixture.registry.ids().is_empty(), "{stage:?}");

    // No running driver: the session (opened for the `Brief` stage) was stopped and joined.
    // `driver_stopped` fires before the join resolves, so a count of one is exact.
    let expected_stops = usize::from(stage == SpawnStage::Brief);
    assert_eq!(
        fixture.stops.count("t1"),
        expected_stops,
        "{stage:?}: a driver is still running"
    );

    // The journal pairs every `TaskSpawned` with a `TaskFinished`.
    let lines = fixture.lead_journal_lines();
    let spawned = lines.iter().filter(|v| v["event"] == "TaskSpawned").count();
    let finished: Vec<&Value> = lines
        .iter()
        .filter(|v| v["event"] == "TaskFinished")
        .collect();
    assert_eq!(
        finished.len(),
        spawned,
        "{stage:?}: {spawned} TaskSpawned but {} TaskFinished",
        finished.len()
    );
    if stage == SpawnStage::TaskSpawned {
        assert_eq!(spawned, 0, "nothing was recorded before the seam");
    } else {
        assert_eq!(spawned, 1, "{stage:?}");
        assert_eq!(finished[0]["data"]["outcome"], "failed", "{stage:?}");
        assert!(
            finished[0]["data"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("test seam")),
            "{stage:?}: the failure reason must be recorded: {}",
            finished[0]
        );
    }
}

// ---------------------------------------------------------------------------------------------
// ADR 0039 decision 7: the end-of-task commit is awaitable and its failure is task state.
// ---------------------------------------------------------------------------------------------

/// `remove` waits for a running end-of-task commit (ADR 0039 decision 7). The commit is held in
/// the gate seam: without the await, `remove` would see the still-uncommitted worktree and
/// refuse, so the "still running" assertion is what the mutation breaks.
#[tokio::test]
async fn remove_waits_for_the_end_of_task_commit() {
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "write", "arguments": "{\"path\":\"made.txt\",\"content\":\"by the task\\n\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        Reply::sse(&[
            chunk(json!({"role": "assistant", "content": "done"}), None),
            chunk(json!({}), Some("stop")),
        ]),
    ])
    .await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let entered_tx = Mutex::new(Some(entered_tx));
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let gate: CommitGate = Arc::new(move || {
        if let Some(entered) = entered_tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = entered.send(());
        }
        // Block the commit until the test releases it: no sleep decides the ordering.
        let _ = release_rx.lock().unwrap_or_else(|e| e.into_inner()).recv();
    });
    let fixture = Fixture::with_seams(
        "remove-wait",
        &server.base_url,
        TaskSeams {
            spawn: None,
            commit_gate: Some(gate),
        },
    );
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let mut request = SpawnRequest::new("t1");
    request.brief = Some("write made.txt".to_owned());
    fixture.registry.spawn(request).await.expect("spawn");

    // The turn ended and its commit is now blocked in the gate.
    entered_rx.await.expect("the end-of-task commit started");
    let mut remove = Box::pin(fixture.registry.remove("t1", false));
    let observed = tokio::time::timeout(std::time::Duration::from_millis(500), &mut remove).await;
    assert!(
        observed.is_err(),
        "remove must wait for the end-of-task commit, not read a half-committed worktree"
    );

    release_tx.send(()).expect("release the commit");
    remove.await.expect("remove succeeds once the commit lands");
    assert!(fixture.registry.get("t1").is_none());
    assert!(
        !fixture.worktree_root().join("t1").exists(),
        "the worktree goes after the commit"
    );
}

/// A failing end-of-task commit is part of the task's result: it lands in a `TaskFinished`
/// (ADR 0039 decision 7), so a reader of the lead's journal sees it.
#[tokio::test]
async fn a_failing_end_commit_shows_in_task_finished() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "done"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let fixture = Fixture::new("commit-fail", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    // Spawn with no brief, so the worktree can leave `radian/t1` before the turn: the end commit
    // then refuses (`wrong_branch`) and the failure must be recorded.
    let record = fixture
        .registry
        .spawn(SpawnRequest::new("t1"))
        .await
        .expect("spawn");
    let worktree = record.worktree.clone().unwrap();
    std::fs::write(worktree.join("change.txt"), "a change\n").unwrap();
    assert!(git(&worktree, &["checkout", "-q", "-b", "other"]));
    record
        .session
        .inbox()
        .followup(
            agent_core::Message::user("go"),
            agent_core::InputSource::Subagent,
        )
        .await
        .expect("the brief");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let finished = loop {
        let lines = fixture.lead_journal_lines();
        if let Some(finished) = lines.iter().find(|v| v["event"] == "TaskFinished") {
            break finished.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the failed end commit never reached TaskFinished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(finished["data"]["outcome"], "completed");
    let error = finished["data"]["error"]
        .as_str()
        .expect("the commit failure is recorded");
    assert!(error.contains("radian/t1"), "{error}");
    assert_eq!(
        git_out(&worktree, &["status", "--porcelain"]),
        "?? change.txt",
        "the failed commit left the change uncommitted"
    );

    // `remove` waits for the (failed) commit and then refuses the dirty worktree.
    let refused = fixture.registry.remove("t1", false).await;
    assert!(refused.is_err(), "a dirty worktree is still refused");
    fixture
        .registry
        .remove("t1", true)
        .await
        .expect("force remove");
}

// ---------------------------------------------------------------------------
// Agent definitions (ADR 0030 O11, slice 7 part a)
// ---------------------------------------------------------------------------

/// Writes one user-level agent definition (`$RADIAN_HOME/agents/<name>.md`).
fn write_agent(home: &Path, name: &str, text: &str) -> PathBuf {
    let dir = home.join("agents");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, text).unwrap();
    path
}

/// Writes one user-level skill (`$RADIAN_HOME/skills/<name>/SKILL.md`) so a task's catalog has
/// something the definition's `skills` list can name (ADR 0025 / ADR 0030 O11).
fn write_skill(home: &Path, name: &str, description: &str) -> PathBuf {
    let dir = home.join("skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("SKILL.md");
    std::fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\nBody.\n"),
    )
    .unwrap();
    path
}

fn agent_tool_call(name: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments: json!({}),
    }
}

/// The system message of the `n`th request the mock saw.
fn system_prompt(server: &MockServer, n: usize) -> String {
    server.requests()[n].json()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The text of the tool result for `call_id` in the `n`-th recorded request.
fn tool_result(server: &MockServer, n: usize, call_id: &str) -> String {
    server.requests()[n].json()["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
                .filter_map(|message| message["content"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// [`Reply`] with one assistant text then `stop`.
fn a_text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// `spawn_task { agent }` resolves the definition and snapshots it; the spawn arguments override
/// the definition's fields (ADR 0030 O4/O11).
#[tokio::test]
async fn a_spawn_resolves_the_agent_definition_and_the_spawn_arguments_override_it() {
    let fixture = Fixture::new("agent-def", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    write_agent(
        &fixture.home,
        "writer-def",
        "---\nname: writer-def\nmodel: opencode-go/one\nprofile: safe\nspawn: true\n---\nBODY\n",
    );

    let mut request = SpawnRequest::new("t1");
    request.agent = Some("writer-def".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    assert_eq!(record.agent, "writer-def");
    assert!(record.spawn_ignored, "spawn: true is ignored for a task");
    assert_eq!(
        record.session.model_route(),
        Some(("opencode-go".to_owned(), "one".to_owned())),
        "the definition's model is the session's route"
    );
    assert_eq!(
        record.session.hooks().profile().preset,
        Preset::Safe,
        "the definition's profile narrows the task"
    );

    // The spawn arguments win over the definition.
    let mut request = SpawnRequest::new("t2");
    request.agent = Some("writer-def".to_owned());
    request.model = Some("anthropic/two".to_owned());
    request.profile = Some("workspace".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    assert_eq!(
        record.session.model_route(),
        Some(("anthropic".to_owned(), "two".to_owned()))
    );
    assert_eq!(
        record.session.hooks().profile().preset,
        Preset::WorkspaceWrite
    );

    fixture.registry.remove("t1", true).await.expect("remove");
    fixture.registry.remove("t2", true).await.expect("remove");
    let _ = std::fs::remove_file(fixture.home.join("agents/writer-def.md"));
}

/// The `tools` allow-list narrows the task (built-ins it does not name are denied) and can never
/// widen a tier: a `safe` task that lists `shell` still has no shell (ADR 0030 problem 7 = A).
#[tokio::test]
async fn a_tools_allow_list_can_only_narrow() {
    let fixture = Fixture::new("agent-tools", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    write_agent(
        &fixture.home,
        "reader",
        "---\nname: reader\ntools: [read, list]\n---\nBODY\n",
    );
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("reader".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    let profile = record.session.hooks().profile();
    let action = |name: &str| {
        radian_permissions::evaluate(profile.rules(), &profile.workspace, &agent_tool_call(name))
            .action
    };
    assert_eq!(action("read"), radian_permissions::Action::Allow);
    assert_eq!(action("list"), radian_permissions::Action::Allow);
    assert_eq!(action("write"), radian_permissions::Action::Deny);
    assert_eq!(action("shell"), radian_permissions::Action::Deny);

    // A `safe` task that lists `shell` keeps the tier's deny: the allow-list cannot widen.
    write_agent(
        &fixture.home,
        "safer",
        "---\nname: safer\nprofile: safe\ntools: [read, shell]\n---\nBODY\n",
    );
    let mut request = SpawnRequest::new("t2");
    request.agent = Some("safer".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    assert!(
        record.sandbox_policy.is_none(),
        "a safe task has no sandbox (the profile denies shell)"
    );
    let profile = record.session.hooks().profile();
    let action = |name: &str| {
        radian_permissions::evaluate(profile.rules(), &profile.workspace, &agent_tool_call(name))
            .action
    };
    assert_eq!(action("read"), radian_permissions::Action::Allow);
    assert_eq!(action("shell"), radian_permissions::Action::Deny);

    fixture.registry.remove("t1", true).await.expect("remove");
    fixture.registry.remove("t2", true).await.expect("remove");
    let _ = std::fs::remove_file(fixture.home.join("agents/reader.md"));
    let _ = std::fs::remove_file(fixture.home.join("agents/safer.md"));
}

/// ADR 0030 O11 / problem 7 = A: the `tools` allow-list is a **physical unmount**, not only a
/// permission deny. A task with `tools: [read, list]` advertises exactly those built-ins to the
/// model and lists exactly those in its `<runtime>` layer; a call to `write` is an unknown tool
/// and executes nothing. `git_commit` is a built-in like any other — the end-of-task commit is
/// host-side and needs no tool — so a list that omits it unmounts it too. The permission `deny`
/// the registry also installs stays beneath the unmount (defence in depth).
#[tokio::test]
async fn a_tools_allow_list_unmounts_the_unlisted_built_ins() {
    // The model's first call is `write`, which the allow-list must have unmounted.
    let server = MockServer::start(vec![
        Reply::sse(&[
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                    "function": {"name": "write", "arguments": "{\"path\":\"made.txt\",\"content\":\"x\\n\"}"}}]}),
                None,
            ),
            chunk(json!({}), Some("tool_calls")),
        ]),
        a_text_reply("done"),
    ])
    .await;
    let fixture = Fixture::new("agent-unmount", &server.base_url);
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    write_agent(
        &fixture.home,
        "unmount-reader",
        "---\nname: unmount-reader\ntools: [read, list]\n---\nBODY\n",
    );
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("unmount-reader".to_owned());
    request.brief = Some("go".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    let worktree = record.worktree.clone().expect("a worktree");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.requests().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The tool specs the model actually received: exactly the allow-list.
    let mut tools: Vec<String> = server.requests()[0].json()["tools"]
        .as_array()
        .expect("the request carries a tools array")
        .iter()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
        .collect();
    tools.sort();
    assert_eq!(
        tools,
        ["list", "read"],
        "only the allow-listed built-ins are mounted"
    );

    // Layer 0a lists what is mounted and nothing else.
    let system = system_prompt(&server, 0);
    assert!(system.contains("- read:"), "{system}");
    assert!(system.contains("- list:"), "{system}");
    assert!(
        !system.contains("- write:"),
        "an unmounted built-in must not be in <runtime>: {system}"
    );

    // Calling the unmounted tool is an unknown tool: it ran nothing and wrote no file.
    let result = tool_result(&server, 1, "c1");
    assert!(
        result.contains("no tool named") && result.contains("write"),
        "{result}"
    );
    assert!(
        !worktree.join("made.txt").exists(),
        "an unmounted tool must not run"
    );

    // The deny rules are kept as defence in depth beside the unmount.
    let profile = record.session.hooks().profile();
    let action = |name: &str| {
        radian_permissions::evaluate(profile.rules(), &profile.workspace, &agent_tool_call(name))
            .action
    };
    assert_eq!(action("read"), radian_permissions::Action::Allow);
    assert_eq!(action("write"), radian_permissions::Action::Deny);

    fixture.registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_file(fixture.home.join("agents/unmount-reader.md"));
}

/// ADR 0030 O11: a definition's `skills` list is the task's skill catalog — every other
/// discovered skill is dropped. Other tasks and the lead keep all of them (absent = all).
#[tokio::test]
async fn a_skills_allow_list_keeps_only_the_named_skills() {
    let fixture = Fixture::new("agent-skills", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let one = write_skill(&fixture.home, "s7a-one", "One.");
    let two = write_skill(&fixture.home, "s7a-two", "Two.");
    write_agent(
        &fixture.home,
        "skills-reader",
        "---\nname: skills-reader\nskills: [s7a-one]\n---\nBODY\n",
    );
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("skills-reader".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");

    let names: Vec<&str> = record
        .session
        .skills()
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["s7a-one"],
        "only the allow-listed skill is in the task's catalog"
    );

    // A task without a `skills` list still sees every discovered skill (absent = all).
    let mut request = SpawnRequest::new("t2");
    request.agent = Some("skills-none".to_owned());
    write_agent(
        &fixture.home,
        "skills-none",
        "---\nname: skills-none\n---\nBODY\n",
    );
    let record = fixture.registry.spawn(request).await.expect("spawn");
    let names: Vec<&str> = record
        .session
        .skills()
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert!(
        names.contains(&"s7a-one") && names.contains(&"s7a-two"),
        "{names:?}"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
    fixture.registry.remove("t2", true).await.expect("remove");
    let _ = std::fs::remove_file(one);
    let _ = std::fs::remove_file(two);
    let _ = std::fs::remove_file(fixture.home.join("agents/skills-reader.md"));
    let _ = std::fs::remove_file(fixture.home.join("agents/skills-none.md"));
}

/// ADR 0030 O11: a `skills` name that does not exist is a spawn-time warning in the result, not
/// a failure. The task opens, its catalog excludes the name, and the missing name is reported.
#[tokio::test]
async fn an_unknown_skill_name_is_a_spawn_warning_not_a_failure() {
    let fixture = Fixture::new("agent-skill-missing", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    write_agent(
        &fixture.home,
        "missing-reader",
        "---\nname: missing-reader\nskills: [s7a-nope]\n---\nBODY\n",
    );
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("missing-reader".to_owned());
    let record = fixture
        .registry
        .spawn(request)
        .await
        .expect("the spawn succeeds");

    assert!(
        record.warnings.iter().any(|w| w.contains("s7a-nope")),
        "the missing skill is a warning: {:?}",
        record.warnings
    );
    assert!(
        !record
            .session
            .skills()
            .iter()
            .any(|skill| skill.name == "s7a-nope"),
        "a missing skill is never invented"
    );

    fixture.registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_file(fixture.home.join("agents/missing-reader.md"));
}

/// Only the user level is read (a project-level definition is not, this slice): a definition
/// that exists only in the lead's workspace is not found.
#[tokio::test]
async fn a_project_level_definition_is_not_read() {
    let fixture = Fixture::new("agent-project", "http://127.0.0.1:1/v1");
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let dir = fixture.lead.join(".radian/agents");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("proj.md"), "---\nname: proj\n---\nBODY\n").unwrap();

    let mut request = SpawnRequest::new("t1");
    request.agent = Some("proj".to_owned());
    let error = match fixture.registry.spawn(request).await {
        Ok(_) => panic!("a project-level definition is not read"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("proj"), "{error}");
}

/// The task worktree for `id` under the isolated home. The repo key is opaque to a test, so it
/// scans the single `worktrees/<repo-key>/` directory.
fn task_worktree(home: &Path, id: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(home.join("worktrees")).ok()? {
        let candidate = entry.ok()?.path().join(id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// The definition is snapshotted at spawn: a change to the file **after** the definition was
/// resolved (here, at the registry's `Builder` seam) does not move the running task's body or
/// its route (ADR 0030 item 4).
#[tokio::test]
async fn a_definition_is_snapshotted_at_spawn() {
    let server = MockServer::start(vec![a_text_reply("done")]).await;
    let home = isolated_home();
    let path = write_agent(
        &home,
        "snap-seam",
        "---\nname: snap-seam\nmodel: opencode-go/old\n---\nOLD BODY\n",
    );
    // The registry resolves the definition before the `Builder` seam; the hook rewrites the file
    // there, so any later re-read (enforce or session open) would see the new text.
    let edit = path.clone();
    let seam = Arc::new(move |stage: SpawnStage| {
        if stage == SpawnStage::Builder {
            std::fs::write(
                &edit,
                "---\nname: snap-seam\nmodel: opencode-go/new\n---\nNEW BODY\n",
            )
            .unwrap();
        }
        Ok(())
    });
    let fixture = Fixture::with_seams(
        "agent-snap",
        &server.base_url,
        TaskSeams {
            spawn: Some(seam),
            commit_gate: None,
        },
    );
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("snap-seam".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    record
        .session
        .inbox()
        .followup(
            agent_core::Message::user("go"),
            agent_core::InputSource::Subagent,
        )
        .await
        .expect("the brief");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.requests().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no model request");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(
        record.session.model_route(),
        Some(("opencode-go".to_owned(), "old".to_owned())),
        "the route is the snapshot, not the edited file"
    );
    let system = system_prompt(&server, 0);
    assert!(system.contains("OLD BODY"), "{system}");
    assert!(!system.contains("NEW BODY"), "{system}");

    fixture.registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_file(home.join("agents/snap-seam.md"));
}

/// A definition with the same name inside the task's **own worktree** is never read: the session
/// takes the lead-time snapshot, and the worktree is a checkout the task can write (ADR 0030
/// revision 2 item 4).
#[tokio::test]
async fn a_definition_in_the_task_worktree_is_not_read() {
    let server = MockServer::start(vec![a_text_reply("done")]).await;
    let home = isolated_home();
    write_agent(&home, "wt-seam", "---\nname: wt-seam\n---\nSNAPSHOT BODY\n");
    // The worktree exists by the `Builder` seam; plant a different definition of the same name
    // there, so a worktree read at enforce or session open would pick it up.
    let plant = home.clone();
    let seam = Arc::new(move |stage: SpawnStage| {
        if stage == SpawnStage::Builder {
            let worktree = task_worktree(&plant, "t1").expect("the worktree exists by Builder");
            let dir = worktree.join(".radian/agents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("wt-seam.md"),
                "---\nname: wt-seam\n---\nWORKTREE BODY\n",
            )
            .unwrap();
        }
        Ok(())
    });
    let fixture = Fixture::with_seams(
        "agent-wt",
        &server.base_url,
        TaskSeams {
            spawn: Some(seam),
            commit_gate: None,
        },
    );
    if workspace_sandbox(&fixture.lead).is_none() {
        return;
    }
    let mut request = SpawnRequest::new("t1");
    request.agent = Some("wt-seam".to_owned());
    let record = fixture.registry.spawn(request).await.expect("spawn");
    record
        .session
        .inbox()
        .followup(
            agent_core::Message::user("go"),
            agent_core::InputSource::Subagent,
        )
        .await
        .expect("the brief");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.requests().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no model request");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let system = system_prompt(&server, 0);
    assert!(system.contains("SNAPSHOT BODY"), "{system}");
    assert!(!system.contains("WORKTREE BODY"), "{system}");

    fixture.registry.remove("t1", true).await.expect("remove");
    let _ = std::fs::remove_file(home.join("agents/wt-seam.md"));
}
