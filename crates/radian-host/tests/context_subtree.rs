//! Subtree context files (ADR 0036): a file appended on first use is an ordinary System
//! message. A compaction summarizes it away and a later read in the same subtree does not
//! append it again.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_core::{NoopSink, TurnOptions};
use radian_host::{
    ApprovalRouting, HostBindings, LocalSession, SessionCapability, SessionCommand, SessionFacade,
    SessionId, SessionSetup, build_model, open_session,
};
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalRequest, Approver, NoopAudit, Preset, Profile,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

struct AllowAll;

impl Approver for AllowAll {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async { Ok(ApprovalOutcome::AllowOnce) })
    }
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

/// A summarizer reply the ADR 0018 check accepts: a `## ` heading line and at least
/// [`radian_core::SUMMARY_MIN_TOKENS`] tokens, so a history that carries the appended block
/// (over `10 * SUMMARY_MIN_TOKENS`) clears the floor.
fn valid_summary() -> String {
    let mut text = String::from("## Goal\nthe work in `crates/x`\n## Progress\n");
    while radian_core::message_tokens(&radian_core::Message::user(&text))
        < radian_core::SUMMARY_MIN_TOKENS
    {
        text.push_str(" more detail");
    }
    text
}

fn read_reply(id: &str, path: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": "read", "arguments": json!({"path": path}).to_string()}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

/// Opens a session with the coding tools mounted against `base_url`.
async fn open(label: &str, base_url: &str, scratch: &Path) -> Arc<LocalSession> {
    let workspace = scratch.join("ws");
    // The workspace is its own repository root, so the checkout's own `AGENTS.md` never
    // joins the startup set.
    std::fs::create_dir_all(workspace.join(".git")).unwrap();
    std::fs::create_dir_all(workspace.join("crates/x/src")).unwrap();
    std::fs::write(
        workspace.join("crates/x/AGENTS.md"),
        format!("SUBTREE-MARKER-X\n{}", "p".repeat(40 * 1024)),
    )
    .unwrap();
    std::fs::write(workspace.join("crates/x/src/a.rs"), "fn a() {}").unwrap();
    std::fs::write(workspace.join("crates/x/src/b.rs"), "fn b() {}").unwrap();

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
            coding_root: Some(workspace.clone()),
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
            subtree_files: true,
            hooks: Default::default(),
            project_hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "mock"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: vec![SessionCapability::Observe, SessionCapability::SendMessage],
        },
        HostBindings {
            approvals: ApprovalRouting::Direct(Arc::new(AllowAll)),
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

/// Waits until the journal is quiescent with `expect_text` committed.
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
        if idle && history.iter().any(|message| message.text() == expect_text) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// How often `needle` appears in the n-th recorded request's conversation messages.
fn conversation_count(server: &MockServer, n: usize, needle: &str) -> usize {
    server.requests()[n].json()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| message.to_string().matches(needle).count())
        .sum()
}

fn context_events(session: &LocalSession) -> usize {
    radian_session::Session::open(session.journal_path())
        .expect("open the journal")
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.data,
                radian_session::EventData::ContextFileAppended { .. }
            )
        })
        .count()
}

/// Edge case 6: an appended block joins the compaction's summary input, is gone from the
/// transcript after the compaction, and a later read in the same subtree appends nothing.
#[tokio::test]
async fn a_compaction_summarizes_an_appended_file_and_it_is_not_appended_again() {
    let scratch = common::TempDir::named("radian-subtree-compact");
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        text_reply("done"),
        text_reply(&valid_summary()),
        read_reply("c2", "crates/x/src/b.rs"),
        text_reply("done again"),
    ])
    .await;
    let session = open("subtree-compact", &server.base_url, &scratch).await;

    send(&session, "go").await;
    wait_quiescent(&session, "done").await;
    wait_requests(&server, 2).await;
    assert_eq!(
        conversation_count(&server, 0, "SUBTREE-MARKER-X"),
        0,
        "the first request has no appended file"
    );
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-MARKER-X"),
        1,
        "the read appended the file before the next request"
    );

    // Compact between turns: the large appended block is older than the last assistant
    // message, so the summary input carries it and the replacement drops it.
    let (tx, rx) = oneshot::channel();
    session.compact_with_budget(
        None,
        Some(4_000),
        Box::new(move |result| {
            let _ = tx.send(result);
        }),
    );
    let outcome = tokio::time::timeout(Duration::from_secs(60), rx)
        .await
        .expect("the compaction ends")
        .expect("the callback runs");
    assert!(outcome.is_ok(), "{outcome:?}");
    wait_requests(&server, 3).await;
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-MARKER-X"),
        1,
        "the summary input carried the appended file"
    );

    send(&session, "again").await;
    wait_quiescent(&session, "done again").await;
    wait_requests(&server, 5).await;
    assert_eq!(
        conversation_count(&server, 3, "SUBTREE-MARKER-X"),
        0,
        "the compacted transcript no longer carries the block"
    );
    assert_eq!(
        conversation_count(&server, 4, "SUBTREE-MARKER-X"),
        0,
        "a later read in the same subtree appends nothing"
    );
    assert_eq!(context_events(&session), 1, "one append, one event");

    session.stop();
    let _ = session.join().await;
}
