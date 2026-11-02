//! ADR 0043: the process control port (`radian-host`'s `control`).
//!
//! Every test drives a real [`ControlServer`] over a real session (a mock model provider), so
//! the socket, the JSON-lines protocol and the source rules are exercised end to end. The
//! socket is bound under a per-test `RADIAN_HOME`; nothing reads the user's `~/.radian`.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{Barrier, MockServer, Reply, chunk};
use radian_coding::{QuestionAnswer, QuestionReply};
use radian_core::{InputSource, Message, NoopSink, TurnOptions};
use radian_host::compose::{Attachment, user_message};
use radian_host::control::MAX_REQUEST_BYTES;
use radian_host::{
    ApprovalRouting, ControlServer, HostBindings, LocalSession, PendingResponse, SessionCommand,
    SessionFacade, SessionId, SessionProjection, SessionSetup, TurnState, build_model,
    open_session,
};
use radian_permissions::{
    Action, ApprovalOutcome, ApprovalPolicy, NoopAudit, Preset, Profile, ResourceMatcher, Rule,
    ToolMatcher,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn scratch(label: &str) -> common::TempDir {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    // A short base: `$TMPDIR` on macOS can push `<home>/run/<pid>.sock` past `SUN_LEN`, and a
    // Unix socket path has a hard length cap. `/tmp` is far below it.
    let dir = PathBuf::from("/tmp").join(format!("radian-ctl-{}-{label}-{n}", std::process::id()));
    common::TempDir::at(dir)
}

/// A private `RADIAN_HOME` for this test binary (skills/context resolution reads it).
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-control-home");
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any session is opened.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
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
        extra_body: Default::default(),
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
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .unwrap();
    radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        slot,
        radian_host::RouteContext {
            extra_body: Default::default(),
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

/// A test event sink that keeps the session projection's turn state current, so
/// [`wait_idle`] means what it says. The TUI's own sink does the same.
struct ProjectionSink {
    projection: Arc<SessionProjection>,
    inner: Arc<dyn radian_core::EventSink>,
}

impl radian_core::EventSink for ProjectionSink {
    fn emit(&self, event: radian_core::AgentEvent) {
        match &event {
            radian_core::AgentEvent::TurnStarted => {
                self.projection.set_turn(TurnState::Running);
            }
            radian_core::AgentEvent::RunFinished { status } => {
                self.projection.set_turn(match status {
                    radian_core::RunStatus::Cancelled(_) => TurnState::Cancelled,
                    _ => TurnState::Idle,
                });
            }
            _ => {}
        }
        self.inner.emit(event);
    }
}

/// A session with the control server bound and a recording event sink (so the `subscribe`
/// fan-out has something to carry).
struct Fixture {
    scratch: common::TempDir,
    home: PathBuf,
    session: Arc<LocalSession>,
    server: Arc<ControlServer>,
    /// The config slot the driver writes: a test can apply a route without a turn
    /// (`ConfigSlot::set_for_test`, the `testing` feature).
    slot: Arc<radian_runtime::ConfigSlot>,
}

/// Whether the fixture asks for approvals and questions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answerable {
    /// `full` with auto approval: no tool ever asks (the existing tests).
    Auto,
    /// `full` with one `ask` rule on `write` and `ask_user` mounted: a real approval or
    /// question queues, and a script can answer it.
    Asking,
}

impl Fixture {
    async fn new(label: &str, base_url: &str) -> Self {
        Self::build(label, base_url, Answerable::Auto).await
    }

    async fn asking(label: &str, base_url: &str) -> Self {
        Self::build(label, base_url, Answerable::Asking).await
    }

    async fn build(label: &str, base_url: &str, answerable: Answerable) -> Self {
        let _ = isolated_home();
        let scratch = scratch(label);
        // A per-test control home: the socket is `<home>/run/<pid>.sock`, and tests run
        // concurrently in one process, so a shared home would collide on the path.
        let home = scratch.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let workspace = scratch.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let session_dir = scratch.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();
        let profile = match answerable {
            Answerable::Auto => Profile::full_access(&workspace)
                .unwrap()
                .with_approval(ApprovalPolicy::Auto),
            Answerable::Asking => Profile::custom(
                Preset::FullAccess,
                ApprovalPolicy::OnAsk,
                &workspace,
                vec![Rule::new(
                    ToolMatcher::Name("write".into()),
                    ResourceMatcher::Any,
                    Action::Ask,
                )],
            )
            .unwrap()
            .with_allowed_tools(&["ask_user"]),
        };
        // The server is bound first so its fan-out sinks can wrap the host's own sinks (ADR
        // 0030 O6): a subscriber sees exactly the committed events the session emits.
        let server = ControlServer::bind(&home).expect("bind the control socket");
        let projection = Arc::new(SessionProjection::new());
        let projection_events = Arc::new(ProjectionSink {
            projection: Arc::clone(&projection),
            inner: Arc::new(NoopSink),
        });
        let events = server.events(projection_events);
        let runtime_events =
            server.runtime_events(Arc::new(|_event: &radian_session::RuntimeEvent| {}));
        let slot = radian_runtime::ConfigSlot::new(Value::Null);
        let session = open_session(
            SessionSetup {
                session_id: SessionId::new(label),
                session_dir: session_dir.clone(),
                workspace: workspace.clone(),
                coding_root: Some(workspace.clone()),
                model: model(label, base_url, Arc::clone(&slot)),
                config_slot: Arc::clone(&slot),
                config_queue: radian_runtime::ConfigQueue::new(),
                budget_for: None,
                price_for: None,
                servers: Vec::new(),
                mcp_config_path: None,
                system_prompt: None,
                context_budget: None,
                window_context: None,
                auto_compact: false,
                compaction_settings: Value::Null,
                compaction_settings_for: None,
                retries: 0,
                sandbox: None,
                tier: Preset::FullAccess,
                profile,
                access_builder: None,
                parent: None,
                subagent: None,
                memory_root: None,
                skills_extra_paths: Vec::new(),
                skills_builtin: true,
                skills_disabled: Vec::new(),
                tools_allow: None,
                extra_deny_read: Vec::new(),
                skills_allow: None,
                memory_injection: radian_host::memory::InjectionConfig::default(),
                subtree_files: true,
                hooks: Default::default(),
                project_hooks: Default::default(),
                trust: radian_host::trust::Setup::untrusted(&workspace, false),
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
                ask_user: answerable == Answerable::Asking,
                audit: Arc::new(|_journal| {
                    Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
                }),
                events,
                projection: Arc::clone(&projection),
                runtime_events,
                driver_stopped: Arc::new(|_| {}),
                before_driver: None,
                memory_warning: Arc::new(|_message: &str| {}),
                hooks_warning: Arc::new(|_message: &str| {}),
                wake_warning: Arc::new(|_message: &str| {}),
                notice_lost_warning: Arc::new(|_message: &str| {}),
                intent_stop_warning: Arc::new(|_message: &str| {}),
                hook_stop_warning: Arc::new(|_message: &str| {}),
            },
        )
        .await
        .expect("open the session");
        let session = Arc::new(session);
        server.serve(
            &tokio::runtime::Handle::current(),
            radian_host::ControlTarget::of(&session),
        );
        Self {
            scratch,
            home,
            session,
            server,
            slot,
        }
    }

    fn socket(&self) -> &Path {
        self.server.path()
    }

    /// Writes the hub owner token under this fixture's own `RADIAN_HOME`.
    fn write_token(&self, token: &str) {
        let dir = radian_host::hub_dir_for(&self.home);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("token"), token).unwrap();
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

/// Sources (ADR 0043, decided item 4): `User` and `PermissionDecision` are refused without
/// the owner token, and a refusal changes nothing (no `InboxEnqueued`); `Scheduler` is accepted
/// and recorded with that source. `cancel` always goes; an approval/answer verb does not exist.
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

/// The one `InboxEnqueued` whose message carries this text.
fn enqueued(journal: &[Value], text: &str) -> Value {
    journal
        .iter()
        .find(|line| {
            line["event"] == "InboxEnqueued"
                && line["data"]["message"]["user"]["content"]
                    .as_array()
                    .is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part["text"]["text"].as_str() == Some(text))
                    })
        })
        .unwrap_or_else(|| panic!("no InboxEnqueued for {text:?}"))
        .clone()
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
/// disconnected while turns keep finishing (ADR 0043: bounded buffer, the runtime never
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

/// ADR 0043 decision 1: `list_sessions` serves one process, so it lists the one `main` row.
#[tokio::test]
async fn list_sessions_returns_the_one_main_row() {
    let server = MockServer::start(vec![text_reply("x")]).await;
    let fixture = Fixture::new("list", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;
    let sessions = client
        .request(json!({"id": 1, "verb": "list_sessions"}))
        .await;
    assert_eq!(sessions["ok"], true, "{sessions}");
    let list = sessions["sessions"].as_array().expect("sessions");
    assert_eq!(list.len(), 1, "{sessions}");
    assert_eq!(list[0]["id"], "list", "{sessions}");
    assert_eq!(list[0]["kind"], "main", "{sessions}");
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

// ---------------------------------------------------------------------------
// The owner token (ADR 0043): the port decides `User`, and the hub origin
// ---------------------------------------------------------------------------

/// A minimal PNG header with these dimensions (33 bytes: the 8-byte signature, the IHDR
/// chunk's length and type, width, height, the bit-depth/colour/compression/filter/interlace
/// bytes, and a CRC). `imagesize` reads only the header, so the rest may be anything.
fn png_photo(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.push(8); // bit depth
    bytes.push(6); // colour type (RGBA)
    bytes.extend_from_slice(&[0, 0, 0]); // compression, filter, interlace
    bytes.extend_from_slice(&[0; 4]); // a CRC the header parser does not check
    bytes
}

/// Edge 1: a `followup` with the right token is the owner's input: source `User`, and the
/// message's `origin` records `{"via":"hub"}` (ADR 0005 G1).
#[tokio::test]
async fn a_followup_with_the_owner_token_is_user_with_the_hub_origin() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let fixture = Fixture::new("owner", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    let response = client
        .request(
            json!({"id": 1, "verb": "followup", "text": "from the phone", "token": "owner-token"}),
        )
        .await;
    assert_eq!(response["ok"], true, "{response}");

    let enqueued = enqueued(&fixture.journal(), "from the phone");
    assert_eq!(enqueued["data"]["source"], "user", "{enqueued}");
    assert_eq!(
        enqueued["data"]["message"]["user"]["meta"]["origin"]["via"], "hub",
        "{enqueued}"
    );
    // The complement of a mid-turn input: with no turn running, this request sets the source
    // of the turn it starts.
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::User),
        "an owner followup that starts a turn sets the turn's source"
    );
    fixture.stop().await;
}

/// Edge 2: a wrong token, a prefix of the right one, and no token at all behave exactly as
/// before: source `Scheduler`, no hub origin.
#[tokio::test]
async fn a_wrong_prefix_or_missing_token_stays_scheduler_without_the_hub_origin() {
    let server = MockServer::start(vec![text_reply("a"), text_reply("b"), text_reply("c")]).await;
    let fixture = Fixture::new("wrong-token", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    for (id, text, token) in [
        (1, "wrong token", Some("wrong")),
        (2, "prefix token", Some("owner")),
        (3, "no token", None),
    ] {
        let mut request = json!({"id": id, "verb": "followup", "text": text});
        if let Some(token) = token {
            request["token"] = json!(token);
        }
        let response = client.request(request).await;
        assert_eq!(response["ok"], true, "{response}");

        let enqueued = enqueued(&fixture.journal(), text);
        assert_eq!(enqueued["data"]["source"], "scheduler", "{enqueued}");
        assert!(
            enqueued["data"]["message"]["user"]["meta"]
                .get("origin")
                .is_none(),
            "a non-owner input records no hub origin: {enqueued}"
        );
    }
    fixture.stop().await;
}

/// Edge 3: with no token file, any `token` is just a wrong token.
#[tokio::test]
async fn without_a_token_file_any_token_stays_scheduler() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let fixture = Fixture::new("no-token-file", &server.base_url).await;
    assert!(
        !radian_host::hub_dir_for(&fixture.home)
            .join("token")
            .exists()
    );
    let mut client = Client::connect(fixture.socket()).await;

    let response = client
        .request(json!({"id": 1, "verb": "followup", "text": "any token", "token": "anything"}))
        .await;
    assert_eq!(response["ok"], true, "{response}");

    let enqueued = enqueued(&fixture.journal(), "any token");
    assert_eq!(enqueued["data"]["source"], "scheduler", "{enqueued}");
    assert!(
        enqueued["data"]["message"]["user"]["meta"]
            .get("origin")
            .is_none(),
        "{enqueued}"
    );
    fixture.stop().await;
}

/// Edge 4: the token file is read trimmed, so its trailing newline does not matter.
#[tokio::test]
async fn a_token_file_with_a_trailing_newline_matches_the_token_without_it() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let fixture = Fixture::new("token-newline", &server.base_url).await;
    fixture.write_token("owner-token\n");
    let mut client = Client::connect(fixture.socket()).await;

    let response = client
        .request(json!({"id": 1, "verb": "followup", "text": "trimmed", "token": "owner-token"}))
        .await;
    assert_eq!(response["ok"], true, "{response}");

    let enqueued = enqueued(&fixture.journal(), "trimmed");
    assert_eq!(enqueued["data"]["source"], "user", "{enqueued}");
    assert_eq!(
        enqueued["data"]["message"]["user"]["meta"]["origin"]["via"], "hub",
        "{enqueued}"
    );
    fixture.stop().await;
}

/// Edge 5: one PNG and one file become the same parts, in the same order, as the TUI path
/// builds through the shared composer (a dropped image loaded by `load_path`, an `@` file).
#[tokio::test]
async fn attachments_become_the_same_parts_as_the_tui_composer() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let fixture = Fixture::new("attachments", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    // The TUI's dropped-image path: a file the composer loads, whose file name becomes the
    // image's display name.
    let image_path = fixture.scratch.join("ws").join("image 1");
    std::fs::write(&image_path, png_photo(1200, 800)).unwrap();
    let radian_coding::image::PathImage::Loaded(image) =
        radian_coding::image::load_path(&image_path)
    else {
        panic!("the dropped image loads");
    };
    let base64 = radian_coding::image::base64_encode(&png_photo(1200, 800));
    let file = Attachment {
        path: "notes.txt".into(),
        content: "hello from the phone".into(),
    };
    let expected = user_message("look", std::slice::from_ref(&file), &[image]);
    let expected = serde_json::to_value(&expected).unwrap();
    let expected_parts = expected["user"]["content"].clone();

    let response = client
        .request(json!({
            "id": 1,
            "verb": "followup",
            "text": "look",
            "images": [{"mime": "image/png", "data": base64}],
            "files": [{"name": "notes.txt", "text": "hello from the phone"}],
        }))
        .await;
    assert_eq!(response["ok"], true, "{response}");

    let journal = fixture.journal();
    let message_id = response["message_id"].as_str().expect("message_id");
    let enqueued = journal
        .iter()
        .find(|line| line["event"] == "InboxEnqueued" && line["data"]["message_id"] == message_id)
        .unwrap_or_else(|| panic!("no InboxEnqueued for {message_id}"));
    assert_eq!(
        enqueued["data"]["message"]["user"]["content"], expected_parts,
        "the control port's parts match the TUI composer's"
    );
    fixture.stop().await;
}

/// Edge 6: one bad attachment rejects the whole request with an error naming it, and enqueues
/// nothing.
#[tokio::test]
async fn a_bad_attachment_names_the_item_and_enqueues_nothing() {
    let server = MockServer::start(vec![]).await;
    let fixture = Fixture::new("bad-attachment", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    let bad_base64 = client
        .request(json!({
            "id": 1,
            "verb": "followup",
            "text": "bad base64",
            "images": [{"mime": "image/png", "data": "not base64!"}],
        }))
        .await;
    assert_eq!(bad_base64["ok"], false, "{bad_base64}");
    assert!(
        bad_base64["error"].as_str().unwrap().contains("image 1"),
        "{bad_base64}"
    );

    let valid_base64 = radian_coding::image::base64_encode(&png_photo(4, 5));
    let bad_mime = client
        .request(json!({
            "id": 2,
            "verb": "followup",
            "text": "bad mime",
            "images": [{"mime": "image/tiff", "data": valid_base64}],
        }))
        .await;
    assert_eq!(bad_mime["ok"], false, "{bad_mime}");
    assert!(
        bad_mime["error"].as_str().unwrap().contains("image 1")
            && bad_mime["error"].as_str().unwrap().contains("image/tiff"),
        "{bad_mime}"
    );

    assert!(
        !fixture
            .journal()
            .iter()
            .any(|line| line["event"] == "InboxEnqueued"),
        "a refused attachment must not reach the inbox"
    );
    fixture.stop().await;
}

/// Edge 7: an over-limit line gets one error line, and the connection stays usable.
#[tokio::test]
async fn an_over_limit_line_gets_an_error_and_the_connection_stays_usable() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let fixture = Fixture::new("line-limit", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    let oversized = "a".repeat(MAX_REQUEST_BYTES + 1024);
    client
        .send(json!({"id": 1, "verb": "followup", "text": oversized}))
        .await;
    let response = client.read().await;
    assert_eq!(response["ok"], false, "{response}");
    assert!(
        response["error"].as_str().unwrap().contains("16 MiB"),
        "{response}"
    );

    // The rest of the over-limit line was discarded through its newline, and the connection
    // still serves requests.
    let response = client
        .request(json!({"id": 2, "verb": "followup", "text": "small"}))
        .await;
    assert_eq!(response["ok"], true, "{response}");
    assert!(
        !fixture
            .journal()
            .iter()
            .any(|line| line["event"] == "InboxEnqueued"
                && line["data"]["message"]["user"]["content"][0]["text"]["text"].as_str()
                    == Some(oversized.as_str())),
        "an over-limit line must not be dispatched"
    );
    fixture.stop().await;
}

/// Edge 8: a token-bearing `steer` during a running turn lands as a `next_step` steer with
/// source `User` (and the hub origin).
#[tokio::test]
async fn a_steer_with_the_token_during_a_running_turn_is_a_user_steer() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("first")),
        },
        text_reply("second"),
        text_reply("third"),
    ])
    .await;
    let fixture = Fixture::new("steer-token", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    let started = client
        .request(json!({"id": 1, "verb": "followup", "text": "start"}))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    // The first model request waits at the barrier, so the turn is provably running: it has
    // started and has not finished.
    wait_requests(&server, 1).await;
    let journal = fixture.journal();
    assert!(
        journal.iter().any(|line| line["event"] == "TurnStarted"),
        "the turn started"
    );
    assert!(
        !journal.iter().any(|line| line["event"] == "TurnFinished"),
        "the turn is still running while the model request waits"
    );

    let steered = client
        .request(json!({"id": 2, "verb": "steer", "text": "turn left", "token": "owner-token"}))
        .await;
    assert_eq!(steered["ok"], true, "{steered}");

    let enqueued = enqueued(&fixture.journal(), "turn left");
    assert_eq!(enqueued["data"]["lane"], "next_step", "{enqueued}");
    assert_eq!(enqueued["data"]["source"], "user", "{enqueued}");
    assert_eq!(
        enqueued["data"]["message"]["user"]["meta"]["origin"]["via"], "hub",
        "{enqueued}"
    );
    fixture.stop().await;
}

/// ADR 0011 L2: a `followup` belongs to the next turn, so a token-bearing one that arrives
/// while a turn runs must not touch that running turn's source — but the turn it opens is the
/// owner's, and runs with `User`. Nothing resets the source at the enqueue, because the turn it
/// belongs to has not started yet.
#[tokio::test]
async fn a_mid_turn_owner_followup_gives_its_own_turn_the_user_source() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("first")),
        },
        text_reply("second"),
    ])
    .await;
    let fixture = Fixture::new("owner-followup-mid-turn", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    // A `Scheduler` input opens the first turn, paused in its first model request.
    let started = client
        .request(json!({"id": 1, "verb": "followup", "text": "start"}))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    wait_requests(&server, 1).await;
    assert!(fixture.session.turn_running(), "the turn is running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a Scheduler input started the turn"
    );

    // The owner's followup is queued for the next turn, not admitted into this one: the
    // running turn keeps its own source.
    let queued = client
        .request(json!({
            "id": 2, "verb": "followup", "text": "owner task", "token": "owner-token"
        }))
        .await;
    assert_eq!(queued["ok"], true, "{queued}");
    assert!(fixture.session.turn_running(), "the turn is still running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a mid-turn owner followup must not raise the running turn's source"
    );

    // The next turn is the one the owner's followup opens: it runs with `User`.
    wait_requests(&server, 2).await;
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::User),
        "the turn the owner's followup opens is the owner's (ADR 0011 L2)"
    );
    let enqueued = enqueued(&fixture.journal(), "owner task");
    assert_eq!(enqueued["data"]["source"], "user", "{enqueued}");
    assert_eq!(
        enqueued["data"]["message"]["user"]["meta"]["origin"]["via"], "hub",
        "{enqueued}"
    );
    fixture.stop().await;
}

/// The mirror of the owner case: a `followup` queued mid-turn belongs to the next turn, so a
/// `Scheduler` one must not lower the running human turn either — and the turn it opens is
/// non-human.
#[tokio::test]
async fn a_mid_turn_scheduler_followup_does_not_lower_the_running_user_turn() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("first")),
        },
        text_reply("second"),
    ])
    .await;
    let fixture = Fixture::new("scheduler-followup-mid-turn", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    // The owner's token opens the first turn, paused in its first model request.
    let started = client
        .request(json!({
            "id": 1, "verb": "followup", "text": "start", "token": "owner-token"
        }))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    wait_requests(&server, 1).await;
    assert!(fixture.session.turn_running(), "the turn is running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::User),
        "the owner's input started a human turn"
    );

    // A scheduled followup is queued for the next turn, not admitted into this one.
    let queued = client
        .request(json!({"id": 2, "verb": "followup", "text": "scheduled task"}))
        .await;
    assert_eq!(queued["ok"], true, "{queued}");
    assert!(fixture.session.turn_running(), "the turn is still running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::User),
        "a mid-turn followup of any source must not change the running turn's source"
    );

    // The scheduled followup's own turn is non-human.
    wait_requests(&server, 2).await;
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "the turn the scheduled followup opens is non-human (ADR 0011 L2)"
    );
    assert_eq!(
        enqueued(&fixture.journal(), "scheduled task")["data"]["source"],
        "scheduler"
    );
    fixture.stop().await;
}

/// The TUI's facade is the other caller that fixes a turn's source. Its `SendFollowup` while a
/// turn runs belongs to the next turn too: it must not raise the running turn's source, and the
/// turn it opens runs with `User` (ADR 0011 L2).
#[tokio::test]
async fn a_facade_followup_during_a_non_human_turn_keeps_it_and_opens_its_own_turn_as_user() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("first")),
        },
        text_reply("second"),
    ])
    .await;
    let fixture = Fixture::new("facade-followup-mid-turn", &server.base_url).await;

    // A `Scheduler` input opens the first turn, paused in its first model request.
    fixture
        .session
        .inbox()
        .followup(Message::user("start"), InputSource::Scheduler)
        .await
        .expect("enqueue the Scheduler input");
    wait_requests(&server, 1).await;
    assert!(fixture.session.turn_running(), "the turn is running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a Scheduler input started the turn"
    );

    // The TUI's send is queued for the next turn. `command` blocks on the inbox, so it runs on
    // a blocking thread, as a real host's synchronous UI thread does.
    let command = Arc::clone(&fixture.session);
    tokio::task::spawn_blocking(move || {
        command.command(SessionCommand::SendFollowup("typed".into()))
    })
    .await
    .expect("the blocking task joins")
    .expect("the send is accepted");
    assert!(fixture.session.turn_running(), "the turn is still running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a mid-turn facade followup must not raise the running turn's source"
    );

    // The next turn is the one the facade followup opens: it is the user's.
    wait_requests(&server, 2).await;
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::User),
        "the turn the facade followup opens is the user's (ADR 0011 L2)"
    );
    fixture.stop().await;
}

/// ADR 0011 L2: a turn's effective source only ever lowers. A token-bearing input that
/// arrives while a non-human turn runs is admitted with source `User`, but it must not raise
/// the running turn's permission source before its safe-point admission: the turn keeps
/// `Scheduler`, and tool permission checks stay non-human.
#[tokio::test]
async fn an_owner_input_does_not_raise_a_running_non_human_turn() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("first")),
        },
        text_reply("second"),
    ])
    .await;
    let fixture = Fixture::new("no-raise", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    // A `Scheduler`-sourced turn is running and paused in its first model request.
    let started = client
        .request(json!({"id": 1, "verb": "followup", "text": "start"}))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    wait_requests(&server, 1).await;
    assert!(fixture.session.turn_running(), "the turn is running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a Scheduler input started the turn"
    );

    // The owner's steer is enqueued with source `User`, but the running turn's effective
    // source must stay where it was: a later human input cannot raise it.
    let steered = client
        .request(json!({"id": 2, "verb": "steer", "text": "turn left", "token": "owner-token"}))
        .await;
    assert_eq!(steered["ok"], true, "{steered}");
    assert!(fixture.session.turn_running(), "the turn is still running");
    assert_eq!(
        fixture.session.hooks().turn_source(),
        Some(InputSource::Scheduler),
        "a mid-turn owner input must not raise the running turn's source (ADR 0011 L2)"
    );
    let enqueued = enqueued(&fixture.journal(), "turn left");
    assert_eq!(enqueued["data"]["source"], "user", "{enqueued}");
    fixture.stop().await;
}

// ---------------------------------------------------------------------------
// The owner verbs (ADR 0043): pending, answer, config, models
// ---------------------------------------------------------------------------

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

fn write_reply(id: &str) -> Reply {
    tool_reply(id, "write", r#"{"path":"out.txt","content":"hello"}"#)
}

fn ask_replies(questions: &Value) -> Vec<Reply> {
    vec![
        tool_reply("call_ask", "ask_user", &questions.to_string()),
        text_reply("done"),
    ]
}

/// The committed tool result for one call id (the transcript's tool message).
fn tool_result(fixture: &Fixture, call_id: &str) -> Value {
    fixture
        .journal()
        .into_iter()
        .find(|line| {
            line["event"] == "MessageAppended"
                && line["data"]["message"]["tool"]["result"]["call_id"] == call_id
        })
        .map(|line| line["data"]["message"]["tool"]["result"].clone())
        .unwrap_or_else(|| {
            panic!(
                "no tool result for {call_id}; journal events: {:?}",
                fixture
                    .journal()
                    .iter()
                    .map(|line| line["event"].clone())
                    .collect::<Vec<_>>()
            )
        })
}

/// A settled result's text parts, joined.
fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"]["text"].as_str())
        .collect::<Vec<_>>()
        .join("")
}

/// Waits until the pending queue holds `want` entries.
async fn wait_queue(fixture: &Fixture, want: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fixture.session.queue().len() != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the queue never reached {want} entries (saw {}); journal: {:?}",
            fixture.session.queue().len(),
            fixture.journal()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Reads lines until one matches, with a watchdog.
async fn read_until(
    client: &mut Client,
    limit: Duration,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let value = tokio::time::timeout_at(deadline, client.read())
            .await
            .expect("a matching event never arrived");
        if matches(&value) {
            return value;
        }
    }
}

/// A logged-in provider for one test, removed on drop. The process home is shared by the
/// tests in one binary, so the lock serializes the writers.
static AUTH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct AuthGuard {
    provider: String,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl AuthGuard {
    fn login(provider: &str, base_url: &str) -> Self {
        // The isolated `RADIAN_HOME` is process-wide and set on first use; set it before the
        // store opens so the test never writes the user's real `~/.radian/credentials`.
        let _ = isolated_home();
        let lock = AUTH_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let mut env = std::collections::BTreeMap::new();
        env.insert(radian_host::RADIAN_BASE_URL.to_owned(), base_url.to_owned());
        radian_host::auth::add_api_key(provider, "sk-test", None, &env).expect("the login writes");
        Self {
            provider: provider.to_owned(),
            _lock: lock,
        }
    }
}

impl Drop for AuthGuard {
    fn drop(&mut self) {
        let _ = radian_host::auth::remove(&self.provider);
    }
}

/// Edge 1: every owner verb without the matching token returns an error and changes nothing.
/// An unauthenticated `answer` with a valid body must not complete the approval.
#[tokio::test]
async fn the_owner_verbs_require_the_token_and_change_nothing() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("owner-gate", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    // The empty queue answers with a null head and a zero length.
    let empty = client
        .request(json!({"id": 0, "verb": "pending", "token": "owner-token"}))
        .await;
    assert_eq!(empty["ok"], true, "{empty}");
    assert_eq!(empty["head"], Value::Null, "{empty}");
    assert_eq!(empty["length"], 0, "{empty}");

    let started = client
        .request(json!({"id": 1, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    wait_queue(&fixture, 1).await;
    let head_id = fixture.session.queue().head().expect("pending").request_id;

    let attempts = [
        json!({"id": 2, "verb": "pending"}),
        json!({"id": 3, "verb": "answer", "request_id": head_id,
               "response": "allow_once"}),
        json!({"id": 4, "verb": "config", "config": {"thinking": "high"}}),
        json!({"id": 5, "verb": "models"}),
    ];
    for request in attempts {
        let refused = client.request(request).await;
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(
            refused["error"].as_str().unwrap().contains("token"),
            "{refused}"
        );
    }
    // A wrong token is no token.
    let refused = client
        .request(json!({"id": 6, "verb": "answer", "request_id": head_id,
                        "response": "allow_once", "token": "wrong"}))
        .await;
    assert_eq!(refused["ok"], false, "{refused}");

    // The approval is still there and the tool never ran.
    assert_eq!(
        fixture.session.queue().len(),
        1,
        "a refused answer changed the queue"
    );
    assert!(
        !fixture.scratch.join("ws").join("out.txt").exists(),
        "a refused answer ran the tool"
    );

    // Clean up: the owner answers and the turn finishes.
    let answered = client
        .request(json!({"id": 7, "verb": "answer", "request_id": head_id,
                        "response": "allow_once", "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    wait_idle(&fixture).await;
    fixture.stop().await;
}

/// Edge 2: an `allow_once` answer through the port runs the tool and empties the queue; a
/// later answer for the same id is the late error.
#[tokio::test]
async fn an_allow_once_answer_from_the_port_runs_the_tool_and_a_later_answer_is_late() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("allow-once", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    client
        .request(json!({"id": 1, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    wait_queue(&fixture, 1).await;

    let pending = client
        .request(json!({"id": 2, "verb": "pending", "token": "owner-token"}))
        .await;
    let head = &pending["head"];
    assert_eq!(head["kind"], "approval", "{pending}");
    assert_eq!(head["tool"], "write", "{pending}");
    assert!(
        head["resolved_resource"]
            .as_str()
            .unwrap()
            .ends_with("out.txt"),
        "{pending}"
    );
    assert_eq!(head["arguments"]["content"], "hello", "{pending}");
    let request_id = head["request_id"].as_u64().expect("the queue id");

    let answered = client
        .request(json!({"id": 3, "verb": "answer", "request_id": request_id,
                        "response": "allow_once", "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    wait_idle(&fixture).await;
    assert_eq!(fixture.session.queue().len(), 0, "the queue is empty");
    assert_eq!(
        std::fs::read_to_string(fixture.scratch.join("ws").join("out.txt")).unwrap(),
        "hello",
        "the approved tool ran"
    );

    let late = client
        .request(json!({"id": 4, "verb": "answer", "request_id": request_id,
                        "response": "allow_once", "token": "owner-token"}))
        .await;
    assert_eq!(late["ok"], false, "{late}");
    assert!(
        late["error"]
            .as_str()
            .unwrap()
            .contains("no longer pending"),
        "the late error says which: {late}"
    );
    fixture.stop().await;
}

/// Edge 3: the in-process approver (the TUI's path) answers first; a port answer for the
/// same id is late and the outcome is unchanged.
#[tokio::test]
async fn an_answer_after_the_in_process_approver_is_late_and_the_outcome_is_unchanged() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("late-answer", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    client
        .request(json!({"id": 1, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    wait_queue(&fixture, 1).await;
    let head = fixture.session.queue().head().expect("pending");
    fixture
        .session
        .answer(
            head.request_id,
            PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .expect("the in-process answer");
    wait_idle(&fixture).await;
    assert_eq!(
        std::fs::read_to_string(fixture.scratch.join("ws").join("out.txt")).unwrap(),
        "hello",
        "the first answer ran the tool"
    );

    let late = client
        .request(
            json!({"id": 2, "verb": "answer", "request_id": head.request_id,
                        "response": "allow_once", "token": "owner-token"}),
        )
        .await;
    assert_eq!(late["ok"], false, "{late}");
    assert!(
        late["error"]
            .as_str()
            .unwrap()
            .contains("no longer pending"),
        "{late}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.scratch.join("ws").join("out.txt")).unwrap(),
        "hello",
        "the late answer did not change the outcome"
    );
    fixture.stop().await;
}

/// Edge 4: `reject_once` with a reason lands that reason in the tool result, exactly like a
/// keyboard reject's reason reaches the permission layer.
#[tokio::test]
async fn a_reject_once_answer_carries_its_reason_into_the_tool_result() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("reject-reason", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    client
        .request(json!({"id": 1, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    wait_queue(&fixture, 1).await;
    let request_id = fixture.session.queue().head().expect("pending").request_id;
    let answered = client
        .request(json!({"id": 2, "verb": "answer", "request_id": request_id,
                        "response": {"kind": "reject_once", "reason": "not now"},
                        "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    wait_idle(&fixture).await;

    let settled = tool_result(&fixture, "call_write");
    let result = &settled;
    assert_eq!(result["is_error"], true, "{settled}");
    assert_eq!(result["error_code"], "denied", "{settled}");
    assert_eq!(result_text(result), "not now", "{settled}");
    assert!(
        !fixture.scratch.join("ws").join("out.txt").exists(),
        "a rejected write never runs"
    );
    fixture.stop().await;
}

/// Edge 5: a free-text question answered through the port lands exactly the tool result a
/// keyboard answer (the in-process `Prompter` path) gives for the same text.
#[tokio::test]
async fn a_free_text_question_answer_through_the_port_equals_a_keyboard_answer() {
    let questions = json!({"questions": [
        {"id": "name", "prompt": "What is it called?", "kind": "text", "required": true}
    ]});

    // Through the port.
    let port_server = MockServer::start(ask_replies(&questions)).await;
    let port = Fixture::asking("ask-port", &port_server.base_url).await;
    port.write_token("owner-token");
    let mut client = Client::connect(port.socket()).await;
    client
        .request(json!({"id": 1, "verb": "followup", "text": "ask", "token": "owner-token"}))
        .await;
    wait_queue(&port, 1).await;
    let pending = client
        .request(json!({"id": 2, "verb": "pending", "token": "owner-token"}))
        .await;
    assert_eq!(pending["head"]["kind"], "question", "{pending}");
    assert_eq!(
        pending["head"]["question"]["questions"][0]["id"], "name",
        "{pending}"
    );
    let request_id = pending["head"]["request_id"].as_u64().unwrap();
    let answered = client
        .request(json!({"id": 3, "verb": "answer", "request_id": request_id,
                        "response": {"answers": [{"id": "name", "text": "radian"}]},
                        "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    wait_idle(&port).await;
    let port_settled = tool_result(&port, "call_ask");

    // The keyboard path: the same `QuestionAnswer` the TUI's form builds.
    let keyboard_server = MockServer::start(ask_replies(&questions)).await;
    let keyboard = Fixture::asking("ask-keyboard", &keyboard_server.base_url).await;
    let mut keyboard_client = Client::connect(keyboard.socket()).await;
    keyboard_client
        .request(json!({"id": 1, "verb": "followup", "text": "ask", "token": "owner-token"}))
        .await;
    wait_queue(&keyboard, 1).await;
    let head = keyboard.session.queue().head().expect("pending");
    keyboard
        .session
        .answer(
            head.request_id,
            PendingResponse::Question(Ok(QuestionAnswer::new(
                format!("q-{}", head.request_id),
                vec![QuestionReply {
                    id: "name".into(),
                    text: Some("radian".into()),
                    ..QuestionReply::default()
                }],
            ))),
        )
        .expect("the keyboard answer");
    wait_idle(&keyboard).await;
    let keyboard_settled = tool_result(&keyboard, "call_ask");

    assert_eq!(
        port_settled["structured"], keyboard_settled["structured"],
        "the port answer lands the keyboard answer's structured result"
    );
    assert_eq!(
        result_text(&port_settled),
        result_text(&keyboard_settled),
        "the human-readable render is the same"
    );
    assert_eq!(
        port_settled["structured"]["request_id"], "q-1",
        "the port derives the TUI's `q-<queue id>` request id"
    );

    port.stop().await;
    keyboard.stop().await;
}

/// Edge 6: `allow_always` is the process-local `AllowForSession` grant, so a second identical
/// request in this process is not queued.
#[tokio::test]
async fn allow_always_stops_a_second_identical_request_from_being_queued() {
    let server = MockServer::start(vec![
        write_reply("call_first"),
        text_reply("one"),
        write_reply("call_second"),
        text_reply("two"),
    ])
    .await;
    let fixture = Fixture::asking("allow-always", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    client
        .request(json!({"id": 1, "verb": "followup", "text": "first", "token": "owner-token"}))
        .await;
    wait_queue(&fixture, 1).await;
    let request_id = fixture.session.queue().head().expect("pending").request_id;
    let answered = client
        .request(json!({"id": 2, "verb": "answer", "request_id": request_id,
                        "response": "allow_always", "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    wait_idle(&fixture).await;
    assert_eq!(
        fixture.session.queue().len(),
        0,
        "the first request is done"
    );

    // The second identical write runs without a queue entry; nothing would answer it if it
    // did queue, so a finished turn is the proof.
    client
        .request(json!({"id": 3, "verb": "followup", "text": "second", "token": "owner-token"}))
        .await;
    wait_requests(&server, 4).await;
    wait_idle(&fixture).await;
    assert_eq!(
        fixture.session.queue().len(),
        0,
        "an `allow_always` grant stops the second question"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.scratch.join("ws").join("out.txt")).unwrap(),
        "hello"
    );
    fixture.stop().await;
}

/// Edge 7: a `config` model switch mid-turn leaves the in-flight request on the old route;
/// the next turn uses the new one and the journal records `ConfigChanged` (ADR 0004 D4).
#[tokio::test]
async fn a_mid_turn_config_model_switch_leaves_the_in_flight_request_on_the_old_route() {
    let barrier = Barrier::new(2, Duration::from_millis(500));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier,
            reply: Box::new(text_reply("one")),
        },
        text_reply("two"),
    ])
    .await;
    let _login = AuthGuard::login("opencode-go", &server.base_url);
    let fixture = Fixture::new("config-switch", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    client
        .request(json!({"id": 1, "verb": "followup", "text": "first", "token": "owner-token"}))
        .await;
    wait_requests(&server, 1).await;
    assert!(fixture.session.turn_running(), "the turn is in flight");

    let changed = client
        .request(json!({"id": 2, "verb": "config", "token": "owner-token",
                        "config": {"model": {"provider": "opencode-go",
                                             "model": "deepseek-v4.1-flash"}}}))
        .await;
    assert_eq!(changed["ok"], true, "{changed}");
    wait_idle(&fixture).await;

    let first: Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    assert_eq!(
        first["model"], "mock",
        "the in-flight request keeps the old route"
    );

    client
        .request(json!({"id": 3, "verb": "followup", "text": "second", "token": "owner-token"}))
        .await;
    wait_requests(&server, 2).await;
    wait_idle(&fixture).await;
    let second: Value = serde_json::from_str(&server.requests()[1].body).unwrap();
    assert_eq!(
        second["model"], "deepseek-v4.1-flash",
        "the next turn uses the new route"
    );

    let change = fixture
        .journal()
        .into_iter()
        .find(|line| line["event"] == "ConfigChanged")
        .expect("a ConfigChanged record");
    assert_eq!(
        change["data"]["change"]["model"]["provider"], "opencode-go",
        "{change}"
    );
    assert_eq!(
        change["data"]["change"]["model"]["model"], "deepseek-v4.1-flash",
        "{change}"
    );
    fixture.stop().await;
}

/// Edge 8: an unknown config key, a bad thinking level and an unknown route each reject the
/// whole request and queue nothing.
#[tokio::test]
async fn a_config_with_an_unknown_key_is_refused_and_queues_nothing() {
    let server = MockServer::start(Vec::new()).await;
    let fixture = Fixture::new("config-bad", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    for (id, config) in [
        (1, json!({"approval": "auto"})),
        (2, json!({"thinking": "turbo"})),
        (3, json!({"model": {"provider": "nope", "model": "x"}})),
        (
            4,
            json!({"model": {"provider": "openai-compatible", "model": "mock"},
                   "thinking": "high", "approval": "auto"}),
        ),
    ] {
        let refused = client
            .request(json!({"id": id, "verb": "config", "token": "owner-token",
                            "config": config}))
            .await;
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(
            fixture.session.config_queue().is_empty(),
            "a refused config queued something: {refused}"
        );
    }
    fixture.stop().await;
}

/// Edge 9: `subscribe` receives a `pending` event when an approval is queued and a `null`
/// head after it is answered. Still committed facts only: no token deltas.
#[tokio::test]
async fn subscribe_receives_the_pending_head_and_a_null_after_the_answer() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("pending-event", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut subscriber = Client::connect(fixture.socket()).await;
    let subscribed = subscriber
        .request(json!({"id": 1, "verb": "subscribe", "token": "owner-token"}))
        .await;
    assert_eq!(subscribed["ok"], true, "{subscribed}");
    let mut driver = Client::connect(fixture.socket()).await;

    driver
        .request(json!({"id": 2, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    let queued = read_until(&mut subscriber, Duration::from_secs(30), |value| {
        value["event"]["name"] == "pending" && value["event"]["data"]["head"] != Value::Null
    })
    .await;
    assert_eq!(queued["event"]["source"], "control", "{queued}");
    assert_eq!(
        queued["event"]["data"]["head"]["kind"], "approval",
        "{queued}"
    );
    assert_eq!(queued["event"]["data"]["head"]["tool"], "write", "{queued}");
    assert_eq!(queued["event"]["data"]["length"], 1, "{queued}");
    let request_id = queued["event"]["data"]["head"]["request_id"]
        .as_u64()
        .expect("the queue id");

    let answered = driver
        .request(json!({"id": 3, "verb": "answer", "request_id": request_id,
                        "response": "allow_once", "token": "owner-token"}))
        .await;
    assert_eq!(answered["ok"], true, "{answered}");
    let cleared = read_until(&mut subscriber, Duration::from_secs(30), |value| {
        value["event"]["name"] == "pending" && value["event"]["data"]["head"] == Value::Null
    })
    .await;
    assert_eq!(cleared["event"]["data"]["length"], 0, "{cleared}");
    wait_idle(&fixture).await;
    fixture.stop().await;
}

/// A subscriber without the token gets the `pending` event, but only the request id, the
/// kind and the tool name: no arguments, no resource, no question text (ADR 0043).
#[tokio::test]
async fn a_subscriber_without_the_token_sees_only_the_redacted_pending_head() {
    let questions = json!({"questions": [
        {"id": "name", "prompt": "Secret question?", "kind": "text", "required": true}
    ]});
    // A question so the test can prove the question text never leaks.
    let server = MockServer::start(ask_replies(&questions)).await;
    let fixture = Fixture::asking("pending-redacted", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut subscriber = Client::connect(fixture.socket()).await;
    let subscribed = subscriber
        .request(json!({"id": 1, "verb": "subscribe"}))
        .await;
    assert_eq!(subscribed["ok"], true, "{subscribed}");
    let mut driver = Client::connect(fixture.socket()).await;
    driver
        .request(json!({"id": 2, "verb": "followup", "text": "ask", "token": "owner-token"}))
        .await;
    let queued = read_until(&mut subscriber, Duration::from_secs(30), |value| {
        value["event"]["name"] == "pending" && value["event"]["data"]["head"] != Value::Null
    })
    .await;
    let head = &queued["event"]["data"]["head"];
    assert_eq!(head["kind"], "question", "{queued}");
    assert_eq!(head["tool"], Value::Null, "{queued}");
    assert!(head["request_id"].as_u64().is_some(), "{queued}");
    assert!(head.get("question").is_none(), "{queued}");
    assert!(
        !queued.to_string().contains("Secret question?"),
        "the question text must not leak: {queued}"
    );

    let request_id = head["request_id"].as_u64().unwrap();
    driver
        .request(json!({"id": 3, "verb": "answer", "request_id": request_id,
                        "response": {"answers": [{"id": "name", "text": "radian"}]},
                        "token": "owner-token"}))
        .await;
    wait_idle(&fixture).await;
    fixture.stop().await;
}

/// Edge 10: `list_sessions` reports the workspace, the model route, `blocked` while an
/// approval waits and `idle` after the turn; none of it needs the token.
#[tokio::test]
async fn list_sessions_reports_blocked_while_an_approval_waits_and_idle_after_the_turn() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("session-state", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    let idle = client
        .request(json!({"id": 0, "verb": "list_sessions"}))
        .await;
    let row = &idle["sessions"][0];
    assert_eq!(row["state"], "idle", "{idle}");
    assert_eq!(row["pending"], 0, "{idle}");
    assert!(row["workspace"].as_str().unwrap().ends_with("ws"), "{idle}");
    assert_eq!(row["model"]["provider"], "openai-compatible", "{idle}");
    assert_eq!(row["model"]["model"], "mock", "{idle}");

    client
        .request(json!({"id": 1, "verb": "followup", "text": "write it", "token": "owner-token"}))
        .await;
    wait_queue(&fixture, 1).await;
    let blocked = client
        .request(json!({"id": 2, "verb": "list_sessions"}))
        .await;
    let row = &blocked["sessions"][0];
    assert_eq!(row["state"], "blocked", "{blocked}");
    assert_eq!(row["pending"], 1, "{blocked}");

    let request_id = fixture.session.queue().head().expect("pending").request_id;
    client
        .request(json!({"id": 3, "verb": "answer", "request_id": request_id,
                        "response": "allow_once", "token": "owner-token"}))
        .await;
    wait_idle(&fixture).await;
    let idle = client
        .request(json!({"id": 4, "verb": "list_sessions"}))
        .await;
    let row = &idle["sessions"][0];
    assert_eq!(row["state"], "idle", "{idle}");
    assert_eq!(row["pending"], 0, "{idle}");
    fixture.stop().await;
}

/// `models`: the `/model` list, grouped by provider, with the current route marked.
#[tokio::test]
async fn models_returns_the_model_list_with_the_current_route_marked() {
    let server = MockServer::start(Vec::new()).await;
    let _login = AuthGuard::login("opencode-go", &server.base_url);
    let fixture = Fixture::new("models", &server.base_url).await;
    fixture.write_token("owner-token");
    let mut client = Client::connect(fixture.socket()).await;

    let listed = client
        .request(json!({"id": 1, "verb": "models", "token": "owner-token"}))
        .await;
    assert_eq!(listed["ok"], true, "{listed}");
    let groups = listed["models"].as_array().expect("groups");
    let opencode = groups
        .iter()
        .find(|group| group["provider"] == "opencode-go")
        .expect("the logged-in provider is listed");
    assert!(
        opencode["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "deepseek-v4.1-flash"),
        "{listed}"
    );

    // Apply the route to the slot (the driver's turn-boundary write, via the test seam): the
    // same route must now be marked.
    fixture.slot.set_for_test(
        json!({"model": {"provider": "opencode-go", "model": "deepseek-v4.1-flash"}}),
    );
    let listed = client
        .request(json!({"id": 2, "verb": "models", "token": "owner-token"}))
        .await;
    let groups = listed["models"].as_array().expect("groups");
    let current = groups
        .iter()
        .find(|group| group["provider"] == "opencode-go")
        .and_then(|group| {
            group["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["id"] == "deepseek-v4.1-flash")
        })
        .expect("the current model row");
    assert_eq!(current["current"], true, "{listed}");
    fixture.stop().await;
}
