//! `/btw`: the side request's body equals the next formal request's body plus
//! one trailing user message (backlog `tui-btw-side-question`).
//!
//! Every test drives a real session against a mock provider, so the request the
//! mock saw is the request the adapter really sent — not a reconstruction.

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{Barrier, MockServer, Reply, chunk};
use radian_core::{NoopSink, TurnOptions};
use radian_host::{
    ApprovalRouting, HostBindings, LocalSession, SessionCapability, SessionCommand, SessionFacade,
    SessionId, SessionSetup, btw, build_model, open_session,
};
use radian_permissions::{NoopAudit, Preset, Profile};
use serde_json::{Value, json};
use tokio::sync::oneshot;

fn scratch(label: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-btw-{label}"))
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

async fn open(label: &str, base_url: &str, scratch: &Path) -> Arc<LocalSession> {
    open_inner(label, base_url, scratch, false).await
}

/// Opens with a paused goal already in the journal: `GoalSet` then `GoalPaused`, so
/// the fold parks it before the first turn and the `<goal>` block rides every request.
async fn open_with_paused_goal(
    label: &str,
    base_url: &str,
    scratch: &Path,
    objective: &str,
) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let journal = radian_session::SessionJournal::create(
        session_dir.join(format!("{label}.jsonl")),
        label,
        radian_session::SessionCreatedData::new(workspace.to_str().unwrap(), json!({})),
    )
    .expect("create the session file");
    let budget = radian_session::GoalState::default_budget();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::GoalSet {
            objective: objective.to_owned(),
            max_turns: budget.max_turns,
            max_duration_secs: budget.max_duration_secs,
            max_tokens: budget.max_tokens,
        })
        .await
        .expect("seed the goal");
    journal
        .append_runtime_event(radian_session::RuntimeEvent::GoalPaused {})
        .await
        .expect("pause the goal");
    drop(journal);
    open_inner(label, base_url, scratch, true).await
}

async fn open_inner(
    label: &str,
    base_url: &str,
    scratch: &Path,
    resume: bool,
) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let slot = radian_runtime::ConfigSlot::new(Value::Null);
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
    let model = radian_host::SwitchableModel::new(
        initial,
        radian_host::ModelRoute::new("openai-compatible", "mock"),
        Arc::clone(&slot),
        radian_host::RouteContext {
            extra_body: Default::default(),
            providers: std::collections::BTreeMap::new(),
            session_id: label.to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    );
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir: session_dir.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model,
            config_slot: slot,
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
            profile: Profile::full_access(&workspace).unwrap(),
            access_builder: None,
            parent: None,
            subagent: None,
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: false,
            skills_disabled: Vec::new(),
            tools_allow: None,
            extra_deny_read: Vec::new(),
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            project_hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: vec![SessionCapability::Observe, SessionCapability::SendMessage],
        },
        HostBindings {
            approvals: ApprovalRouting::Queued,
            shared_queue: None,
            ask_user: false,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(radian_host::SessionProjection::new()),
            runtime_events: Arc::new(|_event: &radian_session::RuntimeEvent| {}),
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
    Arc::new(session)
}

/// `command` blocks on the inbox, so it must not run on a runtime thread (the
/// session.rs pattern: a real host calls it from its synchronous UI thread).
async fn send(session: &Arc<LocalSession>, text: &str) {
    let session = Arc::clone(session);
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || {
        session
            .command(SessionCommand::SendFollowup(text.into()))
            .expect("send the message")
    })
    .await
    .unwrap();
}

async fn wait_requests(server: &MockServer, want: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while server.requests().len() < want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mock never saw {want} requests"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits until the journal is quiescent with `expect_text` committed: the
/// answer is in the committed history **and** the driver is not running a
/// turn (`turn_running` clears after `TurnFinished` is journaled, so nothing
/// more will be written). The projection's turn state is not used: the test
/// fixture's event sink is a `NoopSink`, so nothing forwards turn events to
/// the projection.
async fn wait_quiescent(session: &LocalSession, expect_text: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the journal never went quiescent with {expect_text:?}"
        );
        let idle = !session.turn_running();
        let history = radian_session::Session::open(session.journal_path())
            .expect("open the journal")
            .history()
            .expect("fold the history");
        if idle && history.iter().any(|m| m.text() == expect_text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn journal_bytes(session: &LocalSession) -> Vec<u8> {
    std::fs::read(session.journal_path()).expect("read the journal")
}

/// The overlay sink the tests use: deltas accumulate, `done` resolves the
/// outcome through a oneshot.
struct TestSink {
    deltas: Mutex<String>,
    done_tx: Mutex<Option<oneshot::Sender<btw::SideOutcome>>>,
}

impl TestSink {
    fn new() -> (Arc<Self>, oneshot::Receiver<btw::SideOutcome>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                deltas: Mutex::new(String::new()),
                done_tx: Mutex::new(Some(tx)),
            }),
            rx,
        )
    }
}

impl btw::SideSink for TestSink {
    fn delta(&self, text: &str) {
        self.deltas.lock().unwrap().push_str(text);
    }

    fn done(&self, outcome: btw::SideOutcome) {
        if let Some(tx) = self.done_tx.lock().unwrap().take() {
            let _ = tx.send(outcome);
        }
    }
}

async fn side(session: &Arc<LocalSession>, question: &str) -> (btw::SideOutcome, String) {
    let (sink, rx) = TestSink::new();
    let sink_dyn: Arc<dyn btw::SideSink> = sink.clone();
    let _cancel = session
        .start_side_question(question.to_owned(), sink_dyn)
        .expect("start the side question");
    let outcome = tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .expect("the side request ends")
        .expect("the sink resolves the outcome");
    let deltas = sink.deltas.lock().unwrap().clone();
    (outcome, deltas)
}

/// The `/btw q` request body is the next formal request body with the question
/// swapped in as the last user message: the prefix (system prompt, history,
/// tools, settings) is byte-identical. The transcript and journal are
/// byte-identical after the side request.
#[tokio::test]
async fn the_side_body_is_the_next_formal_body_with_the_question_swapped_in() {
    let server = MockServer::start(vec![
        text_reply("formal-answer"),
        text_reply("side-answer"),
        text_reply("formal-two"),
    ])
    .await;
    let scratch = scratch("byte-equal");
    let session = open("byte-equal", &server.base_url, &scratch).await;

    send(&session, "hello").await;
    wait_requests(&server, 1).await;
    wait_quiescent(&session, "formal-answer").await;
    let before = journal_bytes(&session);

    let (outcome, deltas) = side(&session, "what file was that").await;
    let btw::SideOutcome::Done {
        text,
        tool_calls_dropped,
        ..
    } = outcome
    else {
        panic!("the side request is Done, got {outcome:?}");
    };
    assert_eq!(text, "side-answer");
    assert_eq!(
        deltas, "side-answer",
        "the deltas streamed the whole answer"
    );
    assert!(!tool_calls_dropped);
    assert_eq!(
        journal_bytes(&session),
        before,
        "no transcript or usage record was journaled"
    );

    // The *next* formal turn: its body must be the side body with only the
    // last user message swapped.
    send(&session, "second").await;
    wait_requests(&server, 3).await;
    wait_quiescent(&session, "formal-two").await;

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "two formal calls, one side call");
    let next_formal = requests[2].json();
    let body = requests[1].json();
    let next_messages = next_formal["messages"].as_array().unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages.len() + 1,
        next_messages.len() + 1,
        "both carry the same history plus their own last message: {body} vs {next_formal}"
    );
    assert_eq!(
        &messages[..messages.len() - 1],
        &next_messages[..next_messages.len() - 1],
        "the prefix is byte-identical"
    );
    let trailing = messages.last().unwrap();
    assert_eq!(trailing["role"], "user", "{trailing}");
    let trailing_text = trailing["content"].as_str().unwrap_or_default();
    assert!(trailing_text.contains("what file was that"), "{trailing}");
    assert!(trailing_text.contains(btw::BTW_SUFFIX), "{trailing}");
    assert_eq!(next_messages.last().unwrap()["content"], "second");
    // Everything besides the messages is identical: system prompt, tools,
    // settings, model.
    let mut next_rest = next_formal.clone();
    let mut body_rest = body.clone();
    next_rest["messages"] = Value::Null;
    body_rest["messages"] = Value::Null;
    assert_eq!(body_rest, next_rest, "system/tools/settings are identical");

    session.stop();
    let _ = session.join().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A paused goal rides both bodies: with `GoalSet` + `GoalPaused` in the journal the
/// `<goal>` block is in the formal system prompt (the hook) and in the side shape, so
/// they stay byte-identical. Paused, not active: an active goal would keep sending
/// continuations, so the request counts would never settle.
#[tokio::test]
async fn the_side_body_matches_the_formal_body_with_a_paused_goal() {
    let server = MockServer::start(vec![
        text_reply("formal-answer"),
        text_reply("side-answer"),
        text_reply("formal-two"),
    ])
    .await;
    let scratch = scratch("byte-equal-goal");
    let session =
        open_with_paused_goal("byte-equal-goal", &server.base_url, &scratch, "SIDE-GOAL-9").await;

    send(&session, "hello").await;
    wait_requests(&server, 1).await;
    wait_quiescent(&session, "formal-answer").await;
    let before = journal_bytes(&session);

    let (outcome, deltas) = side(&session, "what file was that").await;
    let btw::SideOutcome::Done {
        text,
        tool_calls_dropped,
        ..
    } = outcome
    else {
        panic!("the side request is Done, got {outcome:?}");
    };
    assert_eq!(text, "side-answer");
    assert_eq!(
        deltas, "side-answer",
        "the deltas streamed the whole answer"
    );
    assert!(!tool_calls_dropped);
    assert_eq!(
        journal_bytes(&session),
        before,
        "no transcript or usage record was journaled"
    );

    send(&session, "second").await;
    wait_requests(&server, 3).await;
    wait_quiescent(&session, "formal-two").await;

    let requests = server.requests();
    assert_eq!(requests.len(), 3, "two formal calls, one side call");
    let next_formal = requests[2].json();
    let body = requests[1].json();
    let next_messages = next_formal["messages"].as_array().unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages.len() + 1,
        next_messages.len() + 1,
        "both carry the same history plus their own last message: {body} vs {next_formal}"
    );
    assert_eq!(
        &messages[..messages.len() - 1],
        &next_messages[..next_messages.len() - 1],
        "the prefix is byte-identical"
    );
    // The `<goal>` block rides both bodies: the formal hook appended it and the side
    // shape rebuilt it in hook order.
    for (name, value) in [("side", &body), ("formal", &next_formal)] {
        let text = serde_json::to_string(value).unwrap();
        assert!(
            text.contains("<goal>") && text.contains("SIDE-GOAL-9"),
            "the {name} body must carry the goal block"
        );
    }
    let mut next_rest = next_formal.clone();
    let mut body_rest = body.clone();
    next_rest["messages"] = Value::Null;
    body_rest["messages"] = Value::Null;
    assert_eq!(body_rest, next_rest, "system/tools/settings are identical");

    session.stop();
    let _ = session.join().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A `/btw` fired mid-turn does not change the turn's request count or result:
/// the barrier only releases when both requests are inside, so `peak() == 2`
/// proves the side request ran while the formal turn was still in flight.
#[tokio::test]
async fn a_mid_turn_side_request_leaves_the_turn_alone() {
    let barrier = Barrier::new(2, Duration::from_secs(30));
    let server = MockServer::start(vec![
        Reply::Barrier {
            barrier: barrier.clone(),
            reply: Box::new(text_reply("formal-answer")),
        },
        Reply::Barrier {
            barrier: barrier.clone(),
            reply: Box::new(text_reply("side-answer")),
        },
    ])
    .await;
    let scratch = scratch("mid-turn");
    let session = open("mid-turn", &server.base_url, &scratch).await;

    send(&session, "hello").await;
    // The formal request is inside the barrier: the turn is necessarily running.
    wait_requests(&server, 1).await;
    let (outcome, _) = side(&session, "quick question").await;
    assert!(
        matches!(outcome, btw::SideOutcome::Done { ref text, .. } if text == "side-answer"),
        "{outcome:?}"
    );
    wait_quiescent(&session, "formal-answer").await;

    assert_eq!(barrier.peak(), 2, "both requests overlapped mid-turn");
    assert_eq!(server.requests().len(), 2, "one formal call, one side call");
    // The turn's own result is untouched: the committed history holds the formal
    // answer, and nothing the side request did committed anything.
    let history = radian_session::Session::open(session.journal_path())
        .unwrap()
        .history()
        .unwrap();
    let texts: Vec<String> = history.iter().map(|m| m.text()).collect();
    assert!(
        history.iter().any(|m| m.text() == "formal-answer"),
        "the formal answer committed: {texts:?}"
    );
    assert!(
        history.iter().all(|m| m.text() != "side-answer"),
        "the side answer never entered the transcript"
    );

    session.stop();
    let _ = session.join().await;
    drop(server);
    let _ = std::fs::remove_dir_all(&scratch);
}
