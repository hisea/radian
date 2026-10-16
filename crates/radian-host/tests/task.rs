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
use std::sync::Arc;

use agent_core::{NoopSink, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    HostBindings, Isolation, LocalSession, SessionError, SessionFacade, SessionId,
    SessionProjection, SessionQueue, SessionSetup, SpawnRequest, TaskBuilder, TaskContext,
    TaskRegistry, TaskSession, build_model, end_of_task_commit, open_session,
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
    radian_host::SwitchableModel::new(
        model,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
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
fn builder(session_dir: PathBuf, base_url: String) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
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
                    model: model(ctx.session_id, &base_url, Arc::clone(&slot)),
                    config_slot: slot,
                    config_queue: radian_runtime::ConfigQueue::new(),
                    budget_for: None,
                    price_for: None,
                    protected_branches: Vec::new(),
                    allowed_filters: None,
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
                    driver_stopped: Arc::new(|_| {}),
                    before_driver: None,
                    memory_warning: Arc::new(|_message: &str| {}),
                    memory_notice_line: Arc::new(|_line: &str| {}),
                },
            })
        },
    )
}

/// The pieces a test keeps: the lead's dir, its journal, its queue, and the registry.
struct Fixture {
    scratch: PathBuf,
    lead: PathBuf,
    session_dir: PathBuf,
    queue: Arc<SessionQueue>,
    registry: TaskRegistry,
}

impl Fixture {
    fn new(label: &str, base_url: &str) -> Self {
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
        let registry = TaskRegistry::new(
            lead.clone(),
            "lead",
            lead_journal,
            Arc::clone(&queue),
            builder(session_dir.clone(), base_url.to_owned()),
        );
        Self {
            scratch,
            lead,
            session_dir,
            queue,
            registry,
        }
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
    assert_eq!(worktree, fixture.lead.join(".radian-worktrees/t1"));
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

/// The task's approvals and questions route to the **lead's** queue (ADR 0030 O2), and the
/// task is a task agent (`/mode` refused) with no memory.
#[tokio::test]
async fn a_task_shares_the_lead_queue_is_a_task_agent_and_has_no_memory() {
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
        Arc::ptr_eq(record.session.queue(), &fixture.queue),
        "the task must use the lead's pending queue"
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
    request.isolation = Isolation::Shared;
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
    request.isolation = Isolation::Shared;
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
    let model = model(session_id, base_url, Arc::clone(&slot));
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

    let wt = scratch("wt-filter-work");
    radian_host::task::create_worktree(&lead, &wt, "radian/wt-filter").expect("worktree add");
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
