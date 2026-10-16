//! ADR 0030 slice 8: the process control port (`radian-host`'s `control`).
//!
//! Every test drives a real [`ControlServer`] over a real session (a mock model provider), so
//! the socket, the JSON-lines protocol, the source rules and the task registry are exercised
//! end to end. The socket is bound under a per-test `RADIAN_HOME`; nothing reads the user's
//! `~/.radian`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_core::{NoopSink, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    ApprovalRouting, ControlServer, HostBindings, LocalSession, SessionError, SessionId,
    SessionProjection, SessionSetup, TaskBuilder, TaskContext, TaskRegistry, TaskSession,
    TurnState, build_model, open_session,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn scratch(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    // A short base: `$TMPDIR` on macOS can push `<home>/run/<pid>.sock` past `SUN_LEN`, and a
    // Unix socket path has a hard length cap. `/tmp` is far below it.
    let dir = PathBuf::from("/tmp").join(format!("radian-ctl-{}-{label}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(&dir).unwrap()
}

/// A private `RADIAN_HOME` for this test binary (skills/context resolution reads it).
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("radian-control-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any session is opened.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn git(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("git runs")
        .success()
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn model(
    session_id: &str,
    base_url: &str,
    slot: Arc<radian_runtime::ConfigSlot>,
) -> radian_host::SwitchableModel {
    let initial = build_model(radian_host::ModelArgs {
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
        initial,
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

/// A task builder with no sandbox: the task's turn only calls the mock and writes nothing, so
/// the worktree task registry accepts it without a sandbox backend. The registry still
/// enforces the task invariants (worktree, `Subagent`, no memory, `task` role).
fn task_builder(session_dir: PathBuf, base_url: String) -> TaskBuilder {
    Arc::new(
        move |ctx: &TaskContext<'_>| -> Result<TaskSession, SessionError> {
            let workspace = ctx.workspace.to_path_buf();
            let fail = |error: String| SessionError::Message(error);
            let slot = radian_runtime::ConfigSlot::new(Value::Null);
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
                    shell_auto_background: None,
                    servers: Vec::new(),
                    role: None,
                    system_prompt: None,
                    context_budget: None,
                    auto_compact: false,
                    compaction_settings: Value::Null,
                    compaction_settings_for: None,
                    retries: 0,
                    sandbox: None,
                    tier: Preset::Safe,
                    profile: Profile::safe(&workspace)
                        .map_err(|e| fail(e.to_string()))?
                        .with_approval(ApprovalPolicy::Auto),
                    access_builder: None,
                    task_agent: false,
                    parent: None,
                    allowed_branch: None,
                    memory_root: None,
                    skills_extra_paths: Vec::new(),
                    skills_builtin: true,
                    skills_disabled: Vec::new(),
                    tools_allow: None,
                    skills_allow: None,
                    git_commit_requires_branch: false,
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
                    approvals: ApprovalRouting::Queued,
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
                    named_builder: None,
                },
            })
        },
    )
}

/// A lead session with the control server bound, the task registry mounted, and a recording
/// event sink (so the `subscribe` fan-out has something to carry).
struct Fixture {
    scratch: PathBuf,
    home: PathBuf,
    lead: PathBuf,
    session: Arc<LocalSession>,
    server: Arc<ControlServer>,
}

impl Fixture {
    async fn new(label: &str, base_url: &str) -> Self {
        let _ = isolated_home();
        let scratch = scratch(label);
        // A per-test control home: the socket is `<home>/run/<pid>.sock`, and tests run
        // concurrently in one process, so a shared home would collide on the path.
        let home = scratch.join("home");
        std::fs::create_dir_all(&home).unwrap();
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
        // The server is bound first so its fan-out sinks can wrap the host's own sinks (ADR
        // 0030 O6): a subscriber sees exactly the committed events the session emits.
        let server = ControlServer::bind(&home).expect("bind the control socket");
        let events = server.events(Arc::new(NoopSink));
        let runtime_events =
            server.runtime_events(Arc::new(|_event: &radian_session::RuntimeEvent| {}));
        let slot = radian_runtime::ConfigSlot::new(Value::Null);
        let session = open_session(
            SessionSetup {
                session_id: SessionId::new(label),
                session_dir: session_dir.clone(),
                workspace: lead.clone(),
                coding_root: Some(lead.clone()),
                model: model(label, base_url, Arc::clone(&slot)),
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
                sandbox: None,
                tier: Preset::Safe,
                profile: Profile::safe(&lead)
                    .unwrap()
                    .with_approval(ApprovalPolicy::Auto),
                access_builder: None,
                task_agent: false,
                parent: None,
                allowed_branch: None,
                memory_root: None,
                skills_extra_paths: Vec::new(),
                skills_builtin: true,
                skills_disabled: Vec::new(),
                tools_allow: None,
                skills_allow: None,
                git_commit_requires_branch: false,
                memory_injection: radian_host::memory::InjectionConfig::default(),
                trust: radian_host::trust::Setup::untrusted(&lead, false),
                resume: false,
                config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
                turn_options: TurnOptions::default(),
                turn_timeout: None,
                wording: radian_host::CLI_WORDING,
                capabilities: radian_host::SessionCapability::ALL.to_vec(),
            },
            HostBindings {
                approvals: ApprovalRouting::Queued,
                shared_queue: None,
                ask_user: false,
                audit: Arc::new(|_journal| {
                    Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                }),
                events,
                projection: Arc::new(SessionProjection::new()),
                runtime_events,
                driver_stopped: Arc::new(|_| {}),
                before_driver: None,
                memory_warning: Arc::new(|_message: &str| {}),
                memory_notice_line: Arc::new(|_line: &str| {}),
                task_builder: Some(task_builder(session_dir.clone(), base_url.to_owned())),
                named_builder: None,
            },
        )
        .await
        .expect("open the lead session");
        let session = Arc::new(session);
        server.serve(
            &tokio::runtime::Handle::current(),
            radian_host::ControlTarget::of(&session),
        );
        Self {
            scratch,
            home,
            lead,
            session,
            server,
        }
    }

    fn socket(&self) -> &Path {
        self.server.path()
    }

    fn registry(&self) -> Arc<TaskRegistry> {
        Arc::clone(self.session.lead_tasks().expect("a lead registry"))
    }

    /// The journal's lines, as JSON values.
    fn journal(&self) -> Vec<Value> {
        std::fs::read_to_string(self.session.journal_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn stop(&self) {
        self.session.stop();
        let _ = self.session.join().await;
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.scratch);
        let _ = &self.home;
        let _ = &self.lead;
    }
}

// ---------------------------------------------------------------------------
// A line client
// ---------------------------------------------------------------------------

struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).await.expect("connect");
        let (read, write) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer: write,
        }
    }

    async fn request(&mut self, value: Value) -> Value {
        self.send(value).await;
        self.read().await
    }

    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.expect("write");
        self.writer.flush().await.expect("flush");
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await.expect("read");
        assert!(n > 0, "the connection closed before a line arrived");
        serde_json::from_str(line.trim_end()).expect("a JSON line")
    }

    /// Reads until EOF, returning whether the server closed the connection within `limit`.
    async fn drain_until_closed(&mut self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let mut line = String::new();
            match tokio::time::timeout_at(deadline, self.reader.read_line(&mut line)).await {
                Ok(Ok(0)) => return true,
                Ok(Ok(_)) => continue,
                Ok(Err(_)) => return true,
                Err(_) => return false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sources (ADR 0030 O6, decided item 4): `User` and `PermissionDecision` are refused, and a
/// refusal changes nothing (no `InboxEnqueued`); `Scheduler` is accepted and recorded with that
/// source. `cancel` always goes; an approval/answer verb does not exist.
#[tokio::test]
async fn sources_are_fixed_by_the_port_and_an_approval_verb_does_not_exist() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let fixture = Fixture::new("sources", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    // A `User` source is refused, and nothing is enqueued.
    let refused = client
        .request(json!({"id": 1, "verb": "followup", "text": "hi", "source": "user"}))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("User"),
        "{refused}"
    );
    // A `PermissionDecision` source is refused too.
    let refused = client
        .request(json!({
            "id": 2, "verb": "followup", "text": "hi", "source": "permission_decision"
        }))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");

    // An approval answer is not a verb: an unknown verb changes nothing.
    let refused = client
        .request(json!({"id": 3, "verb": "approve", "request_id": "x"}))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("unknown verb"),
        "{refused}"
    );

    // Nothing was enqueued by any of the refusals.
    assert!(
        !fixture
            .journal()
            .iter()
            .any(|line| line["event"] == "InboxEnqueued"),
        "a refused input must not reach the inbox"
    );

    // `cancel` always works.
    let cancelled = client.request(json!({"id": 4, "verb": "cancel"})).await;
    assert_eq!(cancelled["ok"], true, "{cancelled}");

    // The default (no `source`) is `Scheduler`, and the journal records exactly that.
    let accepted = client
        .request(json!({"id": 5, "verb": "followup", "text": "hello"}))
        .await;
    assert_eq!(accepted["ok"], true, "{accepted}");
    assert!(
        accepted["message_id"].as_str().is_some(),
        "the enqueue returns its id: {accepted}"
    );
    assert!(
        inbox_sources(&fixture).contains(&"scheduler".to_owned()),
        "the durable record carries the port's source: {:?}",
        inbox_sources(&fixture)
    );

    // A client may declare `Subagent`.
    let accepted = client
        .request(json!({
            "id": 6, "verb": "inject", "text": "note", "source": "subagent"
        }))
        .await;
    assert_eq!(accepted["ok"], true, "{accepted}");
    assert!(
        inbox_sources(&fixture).contains(&"subagent".to_owned()),
        "Subagent is a source a client may declare"
    );

    fixture.stop().await;
}

/// The durable `InboxEnqueued` sources so far.
fn inbox_sources(fixture: &Fixture) -> Vec<String> {
    fixture
        .journal()
        .iter()
        .filter(|line| line["event"] == "InboxEnqueued")
        .filter_map(|line| line["data"]["source"].as_str().map(str::to_owned))
        .collect()
}

/// Waits until the mock has seen at least `want` requests.
async fn wait_requests(server: &MockServer, want: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while server.requests().len() < want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no request reached the mock"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until the session's turn state is idle.
async fn wait_idle(fixture: &Fixture) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fixture.session.projection().read().turn() != TurnState::Idle {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the turn never finished"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// `subscribe` fans committed events out to a client, and a client that never reads is
/// disconnected while turns keep finishing (ADR 0030 O6: bounded buffer, the runtime never
/// waits on a client).
#[tokio::test]
async fn a_slow_subscriber_is_disconnected_and_turns_still_finish() {
    // Enough replies that the flood of turns never runs out.
    let replies: Vec<Reply> = (0..120).map(|_| text_reply("ok")).collect();
    let server = MockServer::start(replies).await;
    let fixture = Fixture::new("slow", &server.base_url).await;

    // The slow subscriber: it subscribes and then never reads.
    let mut slow = Client::connect(fixture.socket()).await;
    let subscribed = slow.request(json!({"id": 1, "verb": "subscribe"})).await;
    assert_eq!(subscribed["ok"], true, "{subscribed}");

    // Drive turns from a second, healthy client, one at a time so the inbox never fills. Each
    // turn emits the committed events; the slow subscriber's bounded buffer overflows and it
    // is disconnected.
    let mut driver = Client::connect(fixture.socket()).await;
    let turns = 24usize;
    for i in 0..turns {
        let response = driver
            .request(json!({"id": i as u64 + 2, "verb": "followup", "text": "go"}))
            .await;
        assert_eq!(response["ok"], true, "{response}");
        // Wait for this turn to reach the mock and finish before starting the next.
        wait_requests(&server, i + 1).await;
        wait_idle(&fixture).await;
    }

    // A turn still completes while the slow subscriber is attached.
    wait_requests(&server, turns).await;
    wait_idle(&fixture).await;

    // The slow subscriber was disconnected: its socket reaches EOF within a bound. (It is
    // never read until here, so this also proves the server never blocked on it.)
    assert!(
        slow.drain_until_closed(Duration::from_secs(30)).await,
        "a subscriber that never reads must be disconnected"
    );

    fixture.stop().await;
}

/// `spawn_task` + `wait_task` over the port go through the same registry the lead tools use.
#[tokio::test]
async fn spawn_task_and_wait_task_over_the_port() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let fixture = Fixture::new("spawn", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    // `as` accepts only the only lead that exists (`main`).
    let refused = client
        .request(json!({
            "id": 1, "verb": "spawn_task", "brief": "x", "as": "dev"
        }))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");

    let spawned = client
        .request(json!({
            "id": 2, "verb": "spawn_task", "brief": "do it", "task": "t1", "as": "main"
        }))
        .await;
    assert_eq!(spawned["ok"], true, "{spawned}");
    assert_eq!(spawned["task"], "t1", "{spawned}");
    assert_eq!(spawned["branch"], "radian/t1", "{spawned}");

    let finished = client
        .request(json!({
            "id": 3, "verb": "wait_task", "task": "t1", "timeout_secs": 30
        }))
        .await;
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["event"], "finished", "{finished}");

    // `list_sessions` names the main session and the live task.
    let sessions = client
        .request(json!({"id": 4, "verb": "list_sessions"}))
        .await;
    let list = sessions["sessions"].as_array().expect("sessions");
    assert!(list.iter().any(|row| row["kind"] == "main"), "{sessions}");
    assert!(
        list.iter()
            .any(|row| row["id"] == "t1" && row["kind"] == "task"),
        "{sessions}"
    );

    fixture.registry().remove("t1", true).await.expect("remove");
    fixture.stop().await;
}

/// Socket permissions (0700 dir, 0600 file), stale-socket cleanup, and removal on drop.
#[tokio::test]
async fn the_socket_is_private_cleans_stale_files_and_is_removed_on_drop() {
    use std::os::unix::fs::PermissionsExt;

    let home = scratch("socket");
    let dir = radian_host::run_dir_for(&home);
    radian_host::ensure_private_dir(&dir).unwrap();
    // A stale socket of a dead pid, a live pid's socket, and a file that is not ours.
    std::fs::write(dir.join("2147483647.sock"), b"").unwrap();
    std::fs::write(dir.join("1.sock"), b"").unwrap();
    std::fs::write(dir.join("notes.txt"), b"keep me").unwrap();

    let server = ControlServer::bind(&home).expect("bind");
    let path = server.path().to_path_buf();

    assert!(
        !dir.join("2147483647.sock").exists(),
        "a stale socket of a dead pid is removed"
    );
    assert!(dir.join("1.sock").exists(), "a live pid's socket is kept");
    assert!(dir.join("notes.txt").exists(), "not our file: kept");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700,
        "the run directory is 0700"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the socket file is 0600"
    );

    drop(server);
    assert!(
        !path.exists(),
        "the socket is removed when the server drops"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// The task registry is only reachable when the session has one: a session with no builder
/// answers `spawn_task` with an error, never a panic.
#[tokio::test]
async fn a_session_without_a_registry_refuses_the_task_verbs() {
    // A session built without a task builder: `lead_tasks()` is `None`.
    let server = MockServer::start(vec![text_reply("x")]).await;
    let _ = isolated_home();
    let scratch = scratch("no-registry");
    let home = scratch.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let slot = radian_runtime::ConfigSlot::new(Value::Null);
    let session = Arc::new(
        open_session(
            SessionSetup {
                session_id: SessionId::new("plain"),
                session_dir: session_dir.clone(),
                workspace: scratch.clone(),
                coding_root: Some(scratch.clone()),
                model: model("plain", &server.base_url, Arc::clone(&slot)),
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
                sandbox: None,
                tier: Preset::Safe,
                profile: Profile::safe(&scratch).unwrap(),
                access_builder: None,
                task_agent: false,
                parent: None,
                allowed_branch: None,
                memory_root: None,
                skills_extra_paths: Vec::new(),
                skills_builtin: true,
                skills_disabled: Vec::new(),
                tools_allow: None,
                skills_allow: None,
                git_commit_requires_branch: false,
                memory_injection: radian_host::memory::InjectionConfig::default(),
                trust: radian_host::trust::Setup::untrusted(&scratch, false),
                resume: false,
                config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
                turn_options: TurnOptions::default(),
                turn_timeout: None,
                wording: radian_host::CLI_WORDING,
                capabilities: radian_host::SessionCapability::ALL.to_vec(),
            },
            HostBindings {
                approvals: ApprovalRouting::Queued,
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
                named_builder: None,
            },
        )
        .await
        .expect("open"),
    );
    let server = ControlServer::bind(&home).expect("bind");
    server.serve(
        &tokio::runtime::Handle::current(),
        radian_host::ControlTarget::of(&session),
    );

    let mut client = Client::connect(server.path()).await;
    let refused = client
        .request(json!({"id": 1, "verb": "spawn_task", "brief": "x"}))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("not a lead"),
        "{refused}"
    );

    session.stop();
    let _ = session.join().await;
    let _ = std::fs::remove_dir_all(&scratch);
}
