//! ADR 0043 §1: `[model.smol]` gives the `explore` child its own model route.
//!
//! Three sessions, one per case, each driven in process against a mock provider with a stub
//! child that appends the argv the tool built (one argument per line, `---` between runs).
//! `general` always names the parent's route; `explore` names the configured one when set
//! and the parent's otherwise — including after a `/model` switch, which `explore` ignores.
//! One test per binary for `RADIAN_HOME`; the scratch directories are unique per test because
//! the cases run in parallel threads.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use agent_core::{InputSource, Message, NoopSink, TurnOptions};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_host::{
    ApprovalRouting, AuthStore, HostBindings, ModelRoute, SessionId, SessionProjection,
    SessionSetup, TurnState, build_model, open_session,
};
use radian_permissions::{ApprovalPolicy, NoopAudit, Preset, Profile};
use serde_json::{Value, json};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The switched route's key lives in the process-global `RADIAN_HOME` auth file, so cases
/// that switch (and the plain case, which shares the home) run one at a time. An async mutex:
/// the guard is held across awaits by design.
fn serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn scratch(label: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-subagent-explore-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The session directory lives here, and the session warns when it is accessible to other
    // users; 0700 is what a real `~/.radian` has.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A private `RADIAN_HOME` (skills, context, `auth.json`), set once before any session opens.
fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-subagent-explore-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any session is opened; the cases only
        // ever upsert the same entry.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

/// An executable stub child that appends the argv the tool built, one argument per line with
/// `---` after each run, so two children in one session stay separable.
fn stub(dir: &Path, argv_path: &Path) -> PathBuf {
    let path = dir.join("child.sh");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" '---' >> '{}'\n",
            argv_path.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

/// The recorded argv, split into one block per child run.
fn argv_blocks(argv_path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(argv_path)
        .unwrap_or_default()
        .split_terminator("---\n")
        .map(|block| block.lines().map(str::to_owned).collect())
        .filter(|block: &Vec<String>| !block.is_empty())
        .collect()
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

/// The startup route's model, built exactly as a host builds it (`build_model`).
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
            session_id: "explore-test".to_owned(),
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

struct Case {
    dir: PathBuf,
    argv_path: PathBuf,
    server: MockServer,
    session: Arc<radian_host::LocalSession>,
}

/// Opens a parent session whose stub child records its argv: `replies` are the mock's answers
/// in order, `smol` is the `[model.smol]` route (`None` follows the parent), and
/// `switch` is a `/model` change queued before the first turn.
async fn open_case(
    label: &str,
    replies: Vec<Reply>,
    smol: Option<ModelRoute>,
    switch: Option<ModelRoute>,
) -> Case {
    let home = isolated_home();
    let dir = scratch(label);
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let argv_path = dir.join("argv.txt");
    let child = stub(&dir, &argv_path);
    let server = MockServer::start(replies).await;
    if switch.is_some() {
        // A switched route resolves its key and endpoint per route (`RouteContext::args_for`),
        // so the parent's own request after the switch must reach this mock too.
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
            session_id: SessionId::new(format!("explore-test-{label}")),
            session_dir: dir.clone(),
            workspace: workspace.clone(),
            // `explore` needs the read tools rooted at a workspace.
            coding_root: Some(workspace.clone()),
            model: model(&server.base_url, Arc::clone(&slot)),
            config_slot: slot,
            config_queue: radian_runtime::ConfigQueue::new(),
            budget_for: None,
            price_for: None,
            shell_auto_background: None,
            servers: Vec::new(),
            mcp_config_path: None,
            system_prompt: None,
            context_budget: None,
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
                smol,
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
        },
    )
    .await
    .expect("open the session");
    let session = Arc::new(session);

    if let Some(route) = switch {
        // The switch a `/model` makes: queued now, applied by the driver at the next turn
        // boundary.
        session.queue_config(route.to_value());
    }
    session
        .inbox()
        .followup(Message::user("go"), InputSource::User)
        .await
        .expect("enqueue");

    Case {
        dir,
        argv_path,
        server,
        session,
    }
}

async fn run_case(case: &Case, requests: usize) -> Vec<Vec<String>> {
    wait_until("the children to run", || {
        argv_blocks(&case.argv_path).len() >= requests - 1
    })
    .await;
    wait_until("the turn to finish", || {
        case.session.projection().read().turn() == TurnState::Idle
            && case.server.requests().len() >= requests
    })
    .await;
    // Read before the session stops: stopping removes the scratch directory below.
    let blocks = argv_blocks(&case.argv_path);
    case.session.stop();
    let _ = case.session.join().await;
    let _ = std::fs::remove_dir_all(&case.dir);
    blocks
}

fn route_of(argv: &[String]) -> (Option<String>, Option<String>) {
    (flag_value(argv, "--provider"), flag_value(argv, "--model"))
}

/// With `[model.smol]` set, the `explore` child runs on it while `general` follows the
/// parent's startup route. The mutations (ignoring the key; applying it to `general` too)
/// fail one of the two assertions.
#[tokio::test]
async fn explore_uses_its_route_while_general_follows_the_parent() {
    let _guard = serial().lock().await;
    let case = open_case(
        "set",
        vec![
            tool_reply(
                "p1",
                "subagent",
                r#"{"agent":"explore","task":"EXPLORE-TASK"}"#,
            ),
            tool_reply("p2", "subagent", r#"{"task":"GENERAL-TASK"}"#),
            text_reply("PARENT-DONE"),
        ],
        Some(ModelRoute::new("cheap-provider", "cheap-model")),
        None,
    )
    .await;
    let blocks = run_case(&case, 3).await;

    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert_eq!(
        route_of(&blocks[0]),
        (Some("cheap-provider".into()), Some("cheap-model".into())),
        "explore must run the configured route: {:?}",
        blocks[0]
    );
    assert_eq!(
        route_of(&blocks[1]),
        (Some("openai-compatible".into()), Some("old".into())),
        "general must follow the parent: {:?}",
        blocks[1]
    );
}

/// Unset, both kinds follow the parent — here after a `/model` switch, so the recorded argv
/// must name the new model twice. The mutation (reading a default route from somewhere else)
/// fails these assertions.
#[tokio::test]
async fn without_a_configured_route_both_kinds_follow_the_parent() {
    let _guard = serial().lock().await;
    let case = open_case(
        "unset",
        vec![
            tool_reply(
                "p1",
                "subagent",
                r#"{"agent":"explore","task":"EXPLORE-TASK"}"#,
            ),
            tool_reply("p2", "subagent", r#"{"task":"GENERAL-TASK"}"#),
            text_reply("PARENT-DONE"),
        ],
        None,
        Some(ModelRoute::new("openai-compatible", "new")),
    )
    .await;
    let blocks = run_case(&case, 3).await;

    assert_eq!(
        case.server.requests()[0].json()["model"],
        json!("new"),
        "the queued route must be the one the turn runs"
    );
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    for (index, argv) in blocks.iter().enumerate() {
        assert_eq!(
            route_of(argv),
            (Some("openai-compatible".into()), Some("new".into())),
            "child {index} must follow the parent after the switch: {argv:?}"
        );
    }
}

/// After a `/model` switch `general` follows the parent while `explore` stays on its
/// configured route.
#[tokio::test]
async fn after_a_switch_general_follows_and_explore_stays() {
    let _guard = serial().lock().await;
    let case = open_case(
        "switch",
        vec![
            tool_reply(
                "p1",
                "subagent",
                r#"{"agent":"explore","task":"EXPLORE-TASK"}"#,
            ),
            tool_reply("p2", "subagent", r#"{"task":"GENERAL-TASK"}"#),
            text_reply("PARENT-DONE"),
        ],
        Some(ModelRoute::new("cheap-provider", "cheap-model")),
        Some(ModelRoute::new("openai-compatible", "new")),
    )
    .await;
    let blocks = run_case(&case, 3).await;

    assert_eq!(
        case.server.requests()[0].json()["model"],
        json!("new"),
        "the queued route must be the one the turn runs"
    );
    assert_eq!(blocks.len(), 2, "{blocks:?}");
    assert_eq!(
        route_of(&blocks[0]),
        (Some("cheap-provider".into()), Some("cheap-model".into())),
        "explore must stay on its route after the switch: {:?}",
        blocks[0]
    );
    assert_eq!(
        route_of(&blocks[1]),
        (Some("openai-compatible".into()), Some("new".into())),
        "general must follow the switch: {:?}",
        blocks[1]
    );
}
