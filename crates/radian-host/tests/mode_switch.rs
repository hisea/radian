//! ADR 0038 slice 3: the host-side mode switch (`SessionFacade::set_mode`), its durable
//! `ConfigChanged`, and the role layer it moves.
//!
//! `set_mode` changes nothing in place: it queues the change the driver applies at the next
//! turn boundary, so layer 1 (the role prompt) is the new one from the next turn's first model
//! request — never mid-turn. The tests here pin that (the change is queued, the mode in force
//! has not moved), the refusal for a task agent and for a fixed layer 1, and — with a mock
//! model — that a switch moves the `<role>` layer and *nothing else* in the system prompt, that
//! the other layers are not recomputed, and that a resume restores the mode.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::{NoopSink, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    HostBindings, LocalSession, Mode, SessionCapability, SessionCommand, SessionFacade, SessionId,
    SessionProjection, SessionSetup, TurnState, build_model, open_session,
};
use radian_permissions::{NoopAudit, Preset, Profile};
use serde_json::json;

struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("radian-host-mode-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn workspace(&self) -> std::path::PathBuf {
        let ws = self.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        ws
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NoopApprover;

impl radian_permissions::Approver for NoopApprover {
    fn decide(
        &self,
        _request: radian_permissions::ApprovalRequest,
    ) -> futures_util::future::BoxFuture<
        '_,
        Result<radian_permissions::ApprovalOutcome, radian_permissions::ApprovalError>,
    > {
        Box::pin(async { Ok(radian_permissions::ApprovalOutcome::Deny { reason: None }) })
    }
}

/// The model for a session: the mock server's URL when the test drives turns, a dead one
/// otherwise (those sessions never call it).
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

/// What a test wants from a session before it opens.
struct Options {
    /// The model server (an empty screen means a dead URL: no turn runs).
    base_url: String,
    task_agent: bool,
    system: Option<String>,
    role: Option<String>,
    resume: bool,
    /// Mount `ask_user` and route its questions through the session's queue.
    ask_user: bool,
}

impl Options {
    fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_owned(),
            task_agent: false,
            system: None,
            role: None,
            resume: false,
            ask_user: false,
        }
    }
}

/// Opens a session over `scratch` and returns it with the config slot the test can inspect.
async fn open(
    scratch: &Scratch,
    label: &str,
    options: Options,
) -> (Arc<LocalSession>, Arc<radian_runtime::ConfigSlot>) {
    let workspace = scratch.workspace();
    let slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: scratch.0.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model: model(label, &options.base_url, Arc::clone(&slot)),
            config_slot: Arc::clone(&slot),
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            protected_branches: Vec::new(),
            allowed_filters: None,
            servers: Vec::new(),
            role: options.role,
            system_prompt: options.system,
            context_budget: None,
            auto_compact: false,
            compaction_settings: serde_json::Value::Null,
            compaction_settings_for: None,
            retries: 0,
            sandbox: None,
            tier: Preset::Safe,
            profile: Profile::safe(&workspace).unwrap(),
            access_builder: None,
            task_agent: options.task_agent,
            parent: None,
            allowed_branch: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            memory_injection: radian_host::memory::InjectionConfig::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: options.resume,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(Arc::new(NoopApprover)),
            shared_queue: None,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
            driver_stopped: Arc::new(|_| {}),
            ask_user: options.ask_user,
            before_driver: None,
            memory_warning: Arc::new(|_message: &str| {}),
            memory_notice_line: Arc::new(|_line: &str| {}),
            task_builder: None,
        },
    )
    .await
    .unwrap();
    (Arc::new(session), slot)
}

/// Sends one user message (`command` blocks, so it must not run on a runtime thread).
async fn send(session: &Arc<LocalSession>, text: &str) {
    let session = Arc::clone(session);
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        session.command(SessionCommand::SendFollowup(agent_core::Message::user(
            text,
        )))
    })
    .await
    .unwrap()
    .expect("the session declares `send`");
}

/// Waits until the turn finished: the driver is idle again and the server saw `want` requests.
async fn wait_turn_end(session: &Arc<LocalSession>, server: &MockServer, want: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let idle = session.projection().read().turn() == TurnState::Idle;
        if idle && server.requests().len() >= want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the turn did not finish: {} requests",
            server.requests().len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The system prompt of the `n`-th recorded request (the mock's messages are the wire form;
/// the system message is the first one).
fn system_prompt(server: &MockServer, n: usize) -> String {
    server.requests()[n].json()["messages"]
        .as_array()
        .expect("messages is an array")
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// One text answer.
fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// One tool call.
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

/// The system prompt with the `<role>` block's body replaced by `ROLE`, so two turns can be
/// compared layer by layer: everything else must be byte-identical.
fn without_role(prompt: &str) -> String {
    let start = prompt.find("<role>\n").expect("a <role> block") + "<role>\n".len();
    let end = prompt.find("\n</role>").expect("the block's end");
    format!("{}ROLE{}", &prompt[..start], &prompt[end..])
}

/// The journal's `ConfigChanged` mode changes, in order.
fn journal_modes(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["event"] == "ConfigChanged")
        .filter_map(|value| value["data"]["change"]["mode"].as_str().map(str::to_owned))
        .collect()
}

/// `set_mode` queues the change for the next turn boundary and moves the UI's projection; the
/// mode **in force** (what the role hook reads from the config slot) has not moved, so no model
/// call — in a running turn or otherwise — can see the new role before the boundary.
#[tokio::test]
async fn set_mode_queues_the_change_and_the_mode_in_force_waits_for_the_turn() {
    let scratch = Scratch::new("queue");
    let (session, slot) = open(&scratch, "queue", Options::new("http://127.0.0.1:1/v1")).await;
    assert_eq!(session.projection().read().mode(), Some("general"));

    session.set_mode(Mode::Lead).expect("general -> lead");
    assert_eq!(
        session.config_queue().len(),
        1,
        "one ConfigChanged per switch"
    );
    assert_eq!(
        session.projection().read().mode(),
        Some("lead"),
        "the UI's mirror moves now"
    );
    assert_eq!(
        Mode::of(&slot.value()),
        Mode::General,
        "the mode in force must not move until the driver applies the change at the next turn"
    );

    session.set_mode(Mode::General).expect("lead -> general");
    assert_eq!(session.config_queue().len(), 2);
    assert_eq!(session.projection().read().mode(), Some("general"));
    assert_eq!(Mode::of(&slot.value()), Mode::General);

    session.stop();
    let _ = session.join().await;
}

/// A task agent's role comes from its brief: `/mode` is refused behind the ADR 0030 flag, and
/// nothing is queued.
#[tokio::test]
async fn a_task_agent_cannot_switch_mode() {
    let scratch = Scratch::new("task");
    let options = Options {
        task_agent: true,
        ..Options::new("http://127.0.0.1:1/v1")
    };
    let (session, slot) = open(&scratch, "task", options).await;

    let refused = session
        .set_mode(Mode::Lead)
        .expect_err("a task agent cannot switch the mode");
    assert!(refused.reason.contains("task agent"), "{}", refused.reason);
    assert!(session.config_queue().is_empty(), "nothing is queued");
    assert_eq!(session.projection().read().mode(), Some("general"));
    assert_eq!(Mode::of(&slot.value()), Mode::General);

    session.stop();
    let _ = session.join().await;
}

/// A session whose layer 1 is fixed (`--system`, or an explicit role) refuses too: the switch
/// could not take effect.
#[tokio::test]
async fn a_session_with_a_fixed_role_layer_cannot_switch_mode() {
    for (label, system, role) in [
        ("fixed-system", Some("SYSTEM PROMPT".to_owned()), None),
        ("fixed-role", None, Some("task".to_owned())),
    ] {
        let scratch = Scratch::new(label);
        let options = Options {
            system,
            role,
            ..Options::new("http://127.0.0.1:1/v1")
        };
        let (session, _slot) = open(&scratch, label, options).await;

        let refused = session
            .set_mode(Mode::Lead)
            .expect_err("a fixed role layer cannot be switched");
        assert!(
            refused.reason.contains("fixed for the whole session"),
            "{}",
            refused.reason
        );
        assert!(session.config_queue().is_empty());

        session.stop();
        let _ = session.join().await;
    }
}

/// The whole slice with a mock model: turn 1 runs as `general`; `/mode lead` between turns;
/// turn 2 runs as `lead` with **only** the `<role>` layer different; the context files are not
/// re-read (another layer is not recomputed); and the switch is journaled.
#[tokio::test]
async fn the_role_layer_switches_at_the_next_turn_and_no_other_layer_moves() {
    let scratch = Scratch::new("switch");
    let workspace = scratch.workspace();
    std::fs::write(
        workspace.join("AGENTS.md"),
        "CONTEXT-MARKER-ONE: the first instruction file.\n",
    )
    .unwrap();
    let server = MockServer::start(vec![
        text_reply("one"),
        text_reply("two"),
        text_reply("three"),
    ])
    .await;

    let (session, _slot) = open(&scratch, "switch", Options::new(&server.base_url)).await;
    send(&session, "first").await;
    wait_turn_end(&session, &server, 1).await;

    let first = system_prompt(&server, 0);
    assert!(
        first.contains("You are radian, a coding agent"),
        "turn 1 runs the general role: {first}"
    );
    assert!(first.contains("CONTEXT-MARKER-ONE"), "{first}");

    // Between the turns: the instruction file changes (layer 2 must NOT be re-read) and the
    // user switches the mode.
    std::fs::write(
        workspace.join("AGENTS.md"),
        "CONTEXT-MARKER-ONE: the first instruction file.\nCONTEXT-MARKER-TWO: added later.\n",
    )
    .unwrap();
    session.set_mode(Mode::Lead).expect("general -> lead");

    send(&session, "second").await;
    wait_turn_end(&session, &server, 2).await;

    let second = system_prompt(&server, 1);
    assert!(
        second.contains("You are a radian lead agent"),
        "turn 2 runs the lead role: {second}"
    );
    assert!(
        !second.contains("You are radian, a coding agent"),
        "the general role must be gone: {second}"
    );
    // Only the role layer moved: 0a, 2 and 3 are byte-identical.
    assert_eq!(
        without_role(&first),
        without_role(&second),
        "only `<role>` may differ across the switch"
    );
    // The context files were read once at session start (ADR 0036 C2): a mode switch is not a
    // `/reload`, so the layer the file feeds did not move.
    assert!(second.contains("CONTEXT-MARKER-ONE"), "{second}");
    assert!(
        !second.contains("CONTEXT-MARKER-TWO"),
        "the switch must not re-read the context files: {second}"
    );

    // The switch is durable: the driver journaled it at the turn boundary.
    let journal = scratch.0.join("switch.jsonl");
    let deadline = Instant::now() + Duration::from_secs(5);
    while journal_modes(&journal) != ["lead".to_owned()] && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        journal_modes(&journal),
        ["lead".to_owned()],
        "the mode switch must be journaled as a ConfigChanged"
    );

    session.stop();
    let _ = session.join().await;
}

/// A resume of the same session runs the mode the journal recorded, with no `/mode` in the new
/// process: the driver's replay puts it back into the config slot before the first model call.
#[tokio::test]
async fn a_resumed_session_runs_the_mode_the_journal_recorded() {
    let scratch = Scratch::new("resume");
    let workspace = scratch.workspace();
    std::fs::write(
        workspace.join("AGENTS.md"),
        "CONTEXT-MARKER-ONE: the first instruction file.\n",
    )
    .unwrap();
    let server = MockServer::start(vec![
        text_reply("one"),
        text_reply("two"),
        text_reply("three"),
    ])
    .await;

    let (session, _slot) = open(&scratch, "resume", Options::new(&server.base_url)).await;
    send(&session, "first").await;
    wait_turn_end(&session, &server, 1).await;
    session.set_mode(Mode::Lead).expect("general -> lead");
    send(&session, "second").await;
    wait_turn_end(&session, &server, 2).await;
    assert!(
        system_prompt(&server, 1).contains("You are a radian lead agent"),
        "the switch reached the model in the first process"
    );

    session.stop();
    let _ = session.join().await;
    drop(session);

    // A new process over the same session file. The context file did not change, so the resumed
    // prompt differs from the first turn's only in the role layer.
    let (resumed, _slot) = open(
        &scratch,
        "resume",
        Options {
            resume: true,
            ..Options::new(&server.base_url)
        },
    )
    .await;
    assert_eq!(resumed.projection().read().mode(), Some("lead"));
    send(&resumed, "third").await;
    wait_turn_end(&resumed, &server, 3).await;

    let third = system_prompt(&server, 2);
    assert!(
        third.contains("You are a radian lead agent"),
        "a resume restores the mode: {third}"
    );
    assert!(
        !third.contains("You are radian, a coding agent"),
        "the general role must be gone after a resume: {third}"
    );
    assert_eq!(
        without_role(&system_prompt(&server, 0)),
        without_role(&third),
        "the resumed prompt differs from the first turn only in the role layer"
    );

    resumed.stop();
    let _ = resumed.join().await;
}

/// A `/mode` inside a **running** turn changes nothing for that turn. The model blocks on
/// `ask_user`; the user switches the mode while it waits; the turn's second model call still
/// carries the old role, and only the next turn is the lead one. This is the mid-turn case the
/// queue exists for (ADR 0026 D5: a change takes effect at a turn boundary, never mid-turn).
#[tokio::test]
async fn a_switch_inside_a_running_turn_changes_nothing_until_the_next_turn() {
    let scratch = Scratch::new("mid-turn");
    let server = MockServer::start(vec![
        // Turn 1: a question, then (after the test answers and switches) the answer text.
        tool_reply(
            "call_1",
            "ask_user",
            r#"{"questions":[{"id":"wait","prompt":"Wait?","kind":"text"}]}"#,
        ),
        text_reply("answered"),
        // Turn 2: the switch is in force.
        text_reply("next"),
    ])
    .await;
    let options = Options {
        ask_user: true,
        ..Options::new(&server.base_url)
    };
    let (session, _slot) = open(&scratch, "mid-turn", options).await;

    send(&session, "first").await;
    // The turn is blocked on the question: wait for it to reach the queue.
    let deadline = Instant::now() + Duration::from_secs(20);
    let pending = loop {
        if let Some(head) = session.pending().head() {
            break head;
        }
        assert!(
            Instant::now() < deadline,
            "the question never reached the queue"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        matches!(pending.request, radian_host::PendingRequest::Question(_)),
        "{:?}",
        pending.request
    );

    // The switch lands mid-turn, and the question is answered so the turn can continue.
    session.set_mode(Mode::Lead).expect("general -> lead");
    session
        .answer(
            pending.request_id,
            radian_host::PendingResponse::Question(Ok(radian_coding::QuestionAnswer::new(
                pending.request_id.to_string(),
                vec![radian_coding::QuestionReply {
                    id: "wait".into(),
                    text: Some("go".into()),
                    ..Default::default()
                }],
            ))),
        )
        .expect("the queued question is answerable");

    wait_turn_end(&session, &server, 2).await;
    let mid_turn = system_prompt(&server, 1);
    assert!(
        mid_turn.contains("You are radian, a coding agent"),
        "the turn's second model call must still run the general role: {mid_turn}"
    );
    assert!(
        !mid_turn.contains("You are a radian lead agent"),
        "the switch must not reach a model call in the same turn: {mid_turn}"
    );

    // The next turn is the lead one.
    send(&session, "second").await;
    wait_turn_end(&session, &server, 3).await;
    let next = system_prompt(&server, 2);
    assert!(
        next.contains("You are a radian lead agent"),
        "the next turn runs the lead role: {next}"
    );

    session.stop();
    let _ = session.join().await;
}
