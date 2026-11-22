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
use radian_core::{InputSource, Message, NoopSink, TurnOptions};
use radian_host::compose::{Attachment, user_message};
use radian_host::control::MAX_REQUEST_BYTES;
use radian_host::{
    ApprovalRouting, ControlServer, HostBindings, LocalSession, SessionId, SessionProjection,
    SessionSetup, TurnState, build_model, open_session,
};
use radian_permissions::{
    Action, ApprovalPolicy, NoopAudit, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
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
    /// The projection the session's event sink folds into (and the control port reads for the
    /// idle-block bit a `list_sessions` row reports).
    projection: Arc<SessionProjection>,
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
        Self::build(label, base_url, Answerable::Auto, true).await
    }

    async fn asking(label: &str, base_url: &str) -> Self {
        Self::build(label, base_url, Answerable::Asking, true).await
    }

    /// A host like `radian exec`: its event sink never folds committed events into the session
    /// projection, so the driver's turn flag is the only live signal `list_sessions` has.
    async fn exec_like(label: &str, base_url: &str) -> Self {
        Self::build(label, base_url, Answerable::Auto, false).await
    }

    async fn build(
        label: &str,
        base_url: &str,
        answerable: Answerable,
        folds_events: bool,
    ) -> Self {
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
        let plain: Arc<dyn radian_core::EventSink> = Arc::new(NoopSink);
        let session_events: Arc<dyn radian_core::EventSink> = if folds_events {
            Arc::new(ProjectionSink {
                projection: Arc::clone(&projection),
                inner: plain,
            })
        } else {
            plain
        };
        let events = server.events(session_events);
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
            projection,
        }
    }

    fn socket(&self) -> &Path {
        self.server.path()
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

/// An input that is merely queued does not set the turn source: the turn it opens (or joins)
/// applies its own source when the claim opens it. A plain `inject` opens no turn by itself,
/// so the source stays unset until some turn admits it at a safe point (ADR 0011 L2). Before,
/// an idle request set the source at the enqueue, which a token-bearing request could raise
/// under a non-human turn that was about to be claimed.
#[tokio::test]
async fn an_idle_inject_does_not_set_the_turn_source_before_a_turn_claims_it() {
    let server = MockServer::start(vec![text_reply("unused")]).await;
    let fixture = Fixture::new("idle-inject-source", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    let response = client
        .request(json!({"id": 1, "verb": "inject", "text": "queued context"}))
        .await;
    assert_eq!(response["ok"], true, "{response}");
    // No turn opens for a plain inject, so nothing has claimed it.
    assert!(!fixture.session.turn_running());
    assert_eq!(
        fixture.session.hooks().turn_source(),
        None,
        "a queued input sets no source before its turn claims it"
    );
    fixture.stop().await;
}

/// Sources (ADR 0043, decided item 4): `User` and `PermissionDecision` are refused without
/// the owner token, and a refusal changes nothing (no `InboxEnqueued`); `Scheduler` is accepted
/// and recorded with that source. `cancel` always goes; `approve` is not a verb (`answer` is
/// the owner's separate one).
#[tokio::test]
async fn sources_are_fixed_by_the_port_and_an_unknown_verb_changes_nothing() {
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

    // `approve` is not a verb (the owner's answer verb is `answer`): an unknown verb changes
    // nothing.
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

/// Waits until the driver's turn flag clears. Needed for a host whose event sink never folds
/// the projection, where [`wait_idle`] would return at once.
async fn wait_not_running(fixture: &Fixture) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while fixture.session.turn_running() {
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
// Attachments
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

    // A present `null` is a wrong-typed field, not an absent one. The port refuses it
    // before anything is enqueued.
    for (id, field) in [(3, "images"), (4, "files")] {
        let response = client
            .request(json!({
                "id": id, "verb": "followup", "text": "null field", field: null
            }))
            .await;
        assert_eq!(response["ok"], false, "{response}");
        assert!(
            response["error"].as_str().unwrap().contains(field),
            "the error names the field: {response}"
        );
    }
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

// ---------------------------------------------------------------------------
// `list_sessions` state
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

/// Edge 10: `list_sessions` reports the workspace, the model route, `blocked` while an
/// approval waits and `idle` after the turn.
#[tokio::test]
async fn list_sessions_reports_blocked_while_an_approval_waits_and_idle_after_the_turn() {
    let server = MockServer::start(vec![write_reply("call_write"), text_reply("done")]).await;
    let fixture = Fixture::asking("session-state", &server.base_url).await;
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

    // A port input is never a human turn and could not ask; the session's own front end
    // starts the human turn whose approval waits.
    fixture
        .session
        .inbox()
        .followup(Message::user("write it"), InputSource::User)
        .await
        .expect("enqueue the User input");
    wait_queue(&fixture, 1).await;
    let blocked = client
        .request(json!({"id": 2, "verb": "list_sessions"}))
        .await;
    let row = &blocked["sessions"][0];
    assert_eq!(row["state"], "blocked", "{blocked}");
    assert_eq!(row["pending"], 1, "{blocked}");

    let request_id = fixture.session.queue().head().expect("pending").request_id;
    // The session answers its own question, as its front end does.
    fixture
        .session
        .queue()
        .answer(
            request_id,
            radian_host::PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
        )
        .expect("the approval is still pending");
    wait_idle(&fixture).await;
    let idle = client
        .request(json!({"id": 4, "verb": "list_sessions"}))
        .await;
    let row = &idle["sessions"][0];
    assert_eq!(row["state"], "idle", "{idle}");
    assert_eq!(row["pending"], 0, "{idle}");
    fixture.stop().await;
}

/// Edge 10's exec half: `list_sessions` reads the turn from the driver's live flag, not only
/// from the projection. An exec-hosted session never folds committed events, so its projection
/// stays `idle` during a turn while the flag is set (ADR 0034 H2).
#[tokio::test]
async fn list_sessions_reports_working_while_an_exec_hosts_turn_runs() {
    let barrier = Barrier::new(2, Duration::from_secs(2));
    let server = MockServer::start(vec![Reply::Barrier {
        barrier,
        reply: Box::new(text_reply("done")),
    }])
    .await;
    let fixture = Fixture::exec_like("exec-session-state", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    let started = client
        .request(json!({"id": 1, "verb": "followup", "text": "go"}))
        .await;
    assert_eq!(started["ok"], true, "{started}");
    // The mock's request is at the barrier, so the turn is provably in flight. Nothing folds
    // `TurnStarted` into this host's projection, exactly as in `radian exec`.
    wait_requests(&server, 1).await;
    assert!(
        fixture.session.turn_running(),
        "the driver says a turn runs"
    );
    assert_eq!(
        fixture.session.projection().read().turn(),
        TurnState::Idle,
        "an exec host never writes the projection"
    );

    let running = client
        .request(json!({"id": 2, "verb": "list_sessions"}))
        .await;
    let row = &running["sessions"][0];
    assert_eq!(row["state"], "working", "{running}");
    assert_eq!(row["pending"], 0, "{running}");

    // The barrier expires, the turn ends, and the flag clears: idle again.
    wait_not_running(&fixture).await;
    let idle = client
        .request(json!({"id": 3, "verb": "list_sessions"}))
        .await;
    assert_eq!(idle["sessions"][0]["state"], "idle", "{idle}");
    fixture.stop().await;
}

/// ADR 0034 H2: an idle block the TUI folds into the projection (a used-up wake budget, a
/// `/goal` budget, a lost job notice, or a stop cap) is `blocked` in a `list_sessions` row, the
/// same state herdr and the footer report.
#[tokio::test]
async fn list_sessions_reports_an_idle_block_as_blocked() {
    let server = MockServer::start(Vec::new()).await;
    let fixture = Fixture::new("idle-block", &server.base_url).await;
    let mut client = Client::connect(fixture.socket()).await;

    fixture.projection.set_idle_blocked(true);
    let blocked = client
        .request(json!({"id": 1, "verb": "list_sessions"}))
        .await;
    assert_eq!(blocked["sessions"][0]["state"], "blocked", "{blocked}");

    // A turn start (and a user message) clears the bit; the row is idle again.
    fixture.projection.set_idle_blocked(false);
    let idle = client
        .request(json!({"id": 2, "verb": "list_sessions"}))
        .await;
    assert_eq!(idle["sessions"][0]["state"], "idle", "{idle}");
    fixture.stop().await;
}
