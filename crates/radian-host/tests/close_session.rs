//! The host's side of the control port's `close` verb (ADR 0043): `close_session` cancels the
//! running turn through the `cancel` path, settles an open tool call as cancelled (ADR 0002
//! E5), abandons pending approvals and questions, and joins the driver.
//!
//! These tests open a real session with no control socket, so they run inside the workspace
//! sandbox; the verb and its socket are covered by `tests/control.rs` and the hub-held process
//! by `radian-exec`'s `tests/hub_close.rs`.

#![cfg(unix)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_core::{Message, NoopSink, TurnOptions};
use radian_host::{
    ApprovalRouting, HostBindings, LocalSession, SessionId, SessionProjection, SessionSetup,
    build_model, open_session,
};
use radian_permissions::{
    Action, ApprovalPolicy, NoopAudit, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use serde_json::{Value, json};

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

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

fn question_reply() -> Reply {
    tool_reply(
        "call_ask",
        "ask_user",
        &json!({"questions": [{
            "id": "name",
            "prompt": "Name?",
            "kind": "single",
            "options": [{"id": "a", "label": "A"}],
            "allow_other": true,
            "required": true
        }]})
        .to_string(),
    )
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

/// A session opened without a control socket. `asking` adds the one `ask` rule on `write` and
/// mounts `ask_user`, so a real approval or question can queue.
async fn session(label: &str, base_url: &str, asking: bool) -> Arc<LocalSession> {
    let scratch = common::process_dir(&format!("radian-close-session-{label}"));
    let workspace = scratch.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let session_dir = scratch.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let profile = if asking {
        Profile::custom(
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
        .with_allowed_tools(&["ask_user"])
    } else {
        Profile::full_access(&workspace)
            .unwrap()
            .with_approval(ApprovalPolicy::Auto)
    };
    let slot = radian_runtime::ConfigSlot::new(Value::Null);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new(label),
            session_dir,
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
            ask_user: asking,
            audit: Arc::new(|_journal| {
                Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>
            }),
            events: Arc::new(NoopSink),
            projection: Arc::new(SessionProjection::new()),
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

fn journal(session: &LocalSession) -> Vec<Value> {
    std::fs::read_to_string(session.journal_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Waits until the journal has a line matching `want`.
async fn wait_journal(session: &LocalSession, want: impl Fn(&Value) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !journal(session).iter().any(&want) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the journal never held the expected line: {:?}",
            journal(session)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A close while an `ask_user` tool call waits: the turn is cancelled, the open call settles
/// as cancelled, and the driver joins.
#[tokio::test]
async fn close_session_cancels_a_running_turn_and_settles_the_open_call_as_cancelled() {
    let server = MockServer::start(vec![question_reply(), text_reply("done")]).await;
    let session = session("close-turn", &server.base_url, true).await;
    session
        .inbox()
        .followup(Message::user("ask"), radian_core::InputSource::Scheduler)
        .await
        .expect("enqueue");
    wait_journal(&session, |line| {
        line["event"] == "ToolExecutionStarted" && line["data"]["call_id"] == "call_ask"
    })
    .await;

    tokio::time::timeout(
        Duration::from_secs(15),
        radian_host::close_session(&session),
    )
    .await
    .expect("the close joined the driver")
    .expect("the driver ended cleanly");

    let lines = journal(&session);
    assert!(
        lines
            .iter()
            .any(|line| line["event"] == "ToolExecutionSettled"
                && line["data"]["call_id"] == "call_ask"
                && line["data"]["result"]["error_code"] == "cancelled"),
        "the open call settles as cancelled: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line["event"] == "TurnFinished" && line["data"]["outcome"] == "cancelled"),
        "the turn is recorded as cancelled: {lines:?}"
    );
}

/// A close while an approval waits: the queue empties and the waiter gets its close outcome.
#[tokio::test]
async fn close_session_abandons_a_pending_approval() {
    let server = MockServer::start(vec![
        tool_reply(
            "call_write",
            "write",
            r#"{"path":"out.txt","content":"hello"}"#,
        ),
        text_reply("done"),
    ])
    .await;
    let session = session("close-approval", &server.base_url, true).await;
    session
        .inbox()
        .followup(Message::user("write it"), radian_core::InputSource::User)
        .await
        .expect("enqueue");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while session.queue().len() != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the approval never queued: {:?}",
            journal(&session)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    tokio::time::timeout(
        Duration::from_secs(15),
        radian_host::close_session(&session),
    )
    .await
    .expect("the close joined the driver")
    .expect("the driver ended cleanly");
    assert!(
        session.queue().is_empty(),
        "the close abandoned the pending approval"
    );
}
