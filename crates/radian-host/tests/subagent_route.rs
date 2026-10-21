//! ADR 0043 decision 2 (gap 2): the child runs the parent's **live** model route, not the one
//! the session started with.
//!
//! The child is a stub shell script that records the argv the tool built; the parent is a real
//! session driven in process against a mock provider. A `/model` switch is a queued
//! `ConfigChanged` the driver applies at the turn boundary, so the test's own model request
//! naming the new model proves the switch happened, and the recorded argv proves the child
//! followed it. One test per binary: this one sets a private `RADIAN_HOME` for the session.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_core::{InputSource, Message, NoopSink, TurnOptions};
use radian_host::{
    ApprovalRouting, AuthStore, HostBindings, ModelRoute, SessionId, SessionProjection,
    SessionSetup, TurnState, build_model, open_session,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use serde_json::{Value, json};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("radian-subagent-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The session directory lives here, and the session warns when it is accessible to other
    // users; 0700 is what a real `~/.radian` has.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A private `RADIAN_HOME` (skills, context, `auth.json`), set once before the session opens.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        // /tmp, not $TMPDIR: on macOS the latter can push <home>/run/<pid>.sock past SUN_LEN (104).
        let dir = PathBuf::from("/tmp")
            .join(format!("radian-subagent-route-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any session is opened; this file has
        // exactly one test.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

/// An executable stub child that records the argv the tool built, one argument per line.
fn stub(dir: &Path, argv_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            argv_path.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn flag_value(argv: &[String], flag: &str) -> Option<String> {
    let index = argv.iter().position(|arg| arg == flag)?;
    argv.get(index + 1).cloned()
}

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

/// The startup route's model, built exactly as a host builds it (`build_model`), so the switch
/// is the only thing that can change the route the mock sees.
fn model(base_url: &str, slot: Arc<radian_runtime::ConfigSlot>) -> radian_host::SwitchableModel {
    let initial = build_model(radian_host::ModelArgs {
        provider: radian_host::Provider::OpenAiCompatible,
        base_url: base_url.to_owned(),
        model: "old".into(),
        api_key: Some("sk-startup".into()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: radian_host::Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        // Not a `chatgpt` route (ADR 0026's field): this test drives the
        // openai-compatible adapter against a mock.
        chatgpt_oauth: false,
    })
    .unwrap();
    radian_host::SwitchableModel::new(
        initial,
        ModelRoute::new("openai-compatible", "old"),
        slot,
        radian_host::RouteContext {
            providers: BTreeMap::new(),
            session_id: "route-test".to_owned(),
            user_headers: Vec::new(),
            include_usage: None,
            max_tokens: 1024,
            thinking: "medium".to_owned(),
            thinking_budget: None,
        },
    )
}

/// The parent never approves; the `subagent` tool is not approval-gated.
struct DenyApprover;

impl radian_permissions::Approver for DenyApprover {
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

/// Waits until `predicate` holds, bounded; the sleep is only the poll interval.
async fn wait_until(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// ADR 0043 1a point 2: after a `/model` switch the child names the new model, because the tool
/// reads the same config slot the driver writes at the turn boundary. The mutation (the startup
/// route, `parts.model`) fails the argv assertion, which is what a dropped `route_slot` — a
/// second copy of the route that can drift — would look like.
#[tokio::test]
async fn the_child_runs_the_parents_live_model_route() {
    let home = isolated_home();
    let dir = scratch();
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let argv_path = dir.join("argv.txt");
    let child = stub(&dir, &argv_path);
    let server = MockServer::start(vec![
        tool_reply("p1", "subagent", r#"{"task":"ROUTE-TASK"}"#),
        text_reply("PARENT-DONE"),
    ])
    .await;
    // A switched route resolves its key and endpoint per route (`RouteContext::args_for`), so
    // the parent's own request after the switch must reach this mock too.
    {
        let mut store = AuthStore::open_at(home.join("credentials").join("auth.json")).unwrap();
        store
            .upsert_api_key(
                "openai-compatible",
                "sk-switched",
                &BTreeMap::from([(
                    radian_host::RADIAN_BASE_URL.to_owned(),
                    server.base_url.clone(),
                )]),
            )
            .unwrap();
    }

    let slot = radian_runtime::ConfigSlot::new(Value::Null);
    let session = open_session(
        SessionSetup {
            session_id: SessionId::new("route-test"),
            session_dir: dir.clone(),
            workspace: workspace.clone(),
            coding_root: None,
            model: model(&server.base_url, Arc::clone(&slot)),
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
            profile: Profile::full_access(&workspace)
                .unwrap()
                .with_approval(ApprovalPolicy::Auto),
            access_builder: None,
            parent: None,
            subagent: Some(radian_host::SubagentParts {
                executable: Some(child),
                provider: "openai-compatible".into(),
                model: "old".into(),
                base_url: Some(server.base_url.clone()),
                ..radian_host::SubagentParts::default()
            }),
            memory_root: None,
            skills_extra_paths: Vec::new(),
            skills_builtin: true,
            skills_disabled: Vec::new(),
            tools_allow: None,
            skills_allow: None,
            memory_injection: radian_host::memory::InjectionConfig::default(),
            hooks: Default::default(),
            project_hooks: Default::default(),
            trust: radian_host::trust::Setup::untrusted(&workspace, false),
            resume: false,
            config_snapshot: json!({"provider": "openai-compatible", "model": "old"}),
            turn_options: TurnOptions::default(),
            turn_timeout: None,
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        },
        HostBindings {
            approvals: ApprovalRouting::Direct(Arc::new(DenyApprover)),
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
            hooks_warning: Arc::new(|_message: &str| {}),
            wake_warning: Arc::new(|_message: &str| {}),
            notice_lost_warning: Arc::new(|_message: &str| {}),
            intent_stop_warning: Arc::new(|_message: &str| {}),
        },
    )
    .await
    .expect("open the session");
    let session = Arc::new(session);

    // The switch a `/model` makes: queued now, applied by the driver at the next turn boundary.
    session.queue_config(ModelRoute::new("openai-compatible", "new").to_value());
    session
        .inbox()
        .followup(Message::user("go"), InputSource::User)
        .await
        .expect("enqueue");

    wait_until("the child's recorded argv", || argv_path.exists()).await;
    wait_until("the turn to finish", || {
        session.projection().read().turn() == TurnState::Idle && server.requests().len() >= 2
    })
    .await;

    // The switch really took effect on the parent's own request...
    assert_eq!(
        server.requests()[0].json()["model"],
        json!("new"),
        "the queued route must be the one the turn runs"
    );
    // ...and the child was told the same route, not the startup one.
    let argv: Vec<String> = std::fs::read_to_string(&argv_path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        flag_value(&argv, "--model").as_deref(),
        Some("new"),
        "the child must run the parent's current model: {argv:?}"
    );
    assert_eq!(
        flag_value(&argv, "--provider").as_deref(),
        Some("openai-compatible"),
        "{argv:?}"
    );

    session.stop();
    let _ = session.join().await;
}
