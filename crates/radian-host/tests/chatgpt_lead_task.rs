//! ADR 0026 slice 3, Build step 5: a lead on `chatgpt` can spawn a task whose
//! model is `opencode-go/deepseek-v4.1-flash` (ADR 0030 O11's agent-definition/spawn-argument
//! route), and each request reaches the right mock with the right auth — never the other
//! one's. Two local mocks only (never `chatgpt.com`/`opencode.ai`); one `#[tokio::test]` (see
//! `tests/chatgpt_headers.rs`'s header comment for why this file has exactly one).
//!
//! The model factory below mirrors `radian-tui/src/cli.rs`'s real `task_model` closure
//! exactly: `agent_model: Some(route)` goes through `RouteContext::args_for` + `build_model`
//! (the same function `/model` switching uses — confirmed by reading that closure, ADR 0026
//! revision 1 slice 3's progress note); `agent_model: None` is the lead's own default route.
//! Nothing new is built for this slice's routing; only the fixture is new.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use agent_core::TurnOptions;
use agent_provider_openai_compatible::testing::{
    MockServer as ChatMockServer, Reply as ChatReply, chunk,
};
use agent_provider_openai_responses::testing::{
    MockServer as ResponsesMockServer, Reply as ResponsesReply, completed, message_item,
    output_item_done,
};
use radian_host::{
    AuthStore, ModelArgs, ModelRoute, OAuthEntry, Provider, RouteContext, SpawnRequest,
    SwitchableModel, TaskHost, TaskRegistry, Thinking, build_model, task_builder,
};
use radian_permissions::NoopAudit;
use radian_session::{RuntimeEvent, SessionCreatedData, SessionJournal, TurnOutcome};
use serde_json::json;

fn isolated_home() -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-chatgpt-lead-task-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once; this file has exactly one `#[tokio::test]`.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-chatgpt-lead-task-{label}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

fn fake_access_token(account_id: &str) -> String {
    let header = base64url(br#"{"alg":"none"}"#);
    let payload = base64url(
        format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#)
            .as_bytes(),
    );
    format!("{header}.{payload}.sig")
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn wait_outcome(registry: &Arc<TaskRegistry>, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let record = registry.get(id).expect("the task");
        if !record.running() && record.last_outcome().is_some() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the task's turn never ended"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_chatgpt_lead_default_and_a_deepseek_task_route_requests_to_the_right_mock() {
    let home = isolated_home();
    let access = fake_access_token("acct-lead-task-test");

    // The chatgpt (lead) mock and the opencode-go (task) mock — never confused with each
    // other if the model factory's branches are ever swapped.
    let chatgpt_mock = ResponsesMockServer::start(vec![ResponsesReply::sse(&[
        output_item_done(0, message_item("msg_1", "chatgpt answered")),
        completed(None),
    ])])
    .await;
    let opencode_mock = ChatMockServer::start(vec![ChatReply::sse(&[
        chunk(
            json!({"role": "assistant", "content": "deepseek answered"}),
            None,
        ),
        chunk(json!({}), Some("stop")),
    ])])
    .await;

    {
        let mut store = AuthStore::open_at(home.join("credentials").join("auth.json")).unwrap();
        store
            .upsert_oauth(
                "chatgpt",
                &OAuthEntry {
                    access: access.clone(),
                    refresh: "refresh-unused".into(),
                    expires_at_millis: now_millis() + 3_600_000,
                },
            )
            .unwrap();
        let mut env = std::collections::BTreeMap::new();
        env.insert(
            radian_host::RADIAN_BASE_URL.to_owned(),
            opencode_mock.base_url.clone(),
        );
        store
            .upsert_api_key("opencode-go", "sk-task-key", &env)
            .unwrap();
    }

    // The lead workspace (a git repo; the registry needs one).
    let scratch_dir = scratch("fixture");
    let lead = scratch_dir.join("lead");
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
    let session_dir = scratch_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let journal = Arc::new(
        SessionJournal::create(
            session_dir.join("lead.jsonl"),
            "lead",
            SessionCreatedData::new(lead.to_string_lossy(), json!({})),
        )
        .expect("the lead journal"),
    );
    let queue = Arc::new(radian_host::SessionQueue::new());

    let chatgpt_base_url = chatgpt_mock.base_url.clone();
    let model_factory: radian_host::TaskModelFactory = Arc::new(
        move |session_id: &str,
              slot: Arc<radian_runtime::ConfigSlot>,
              agent_model: Option<ModelRoute>,
              agent_thinking: Option<String>| {
            let ctx = RouteContext {
                providers: std::collections::BTreeMap::new(),
                session_id: session_id.to_owned(),
                user_headers: Vec::new(),
                include_usage: None,
                max_tokens: 4096,
                thinking: agent_thinking.unwrap_or_else(|| "medium".to_owned()),
                thinking_budget: None,
            };
            // This mirrors `radian-tui/src/cli.rs`'s real `task_model` closure: an agent's
            // own route goes through the *same* `args_for`/`build_model` a `/model` switch
            // uses; no agent route is the lead's own default.
            if let Some(route) = agent_model {
                let args = ctx.args_for(&route.provider, &route.model)?;
                let initial = build_model(args).map_err(|error| error.to_string())?;
                return Ok(SwitchableModel::new(initial, route, slot, ctx));
            }
            let initial = build_model(ModelArgs {
                provider: Provider::ChatGpt,
                base_url: chatgpt_base_url.clone(),
                model: "gpt-5.4".to_owned(),
                api_key: None,
                headers: vec![("originator".to_owned(), "radian".to_owned())],
                include_usage: None,
                max_tokens: 4096,
                thinking: Thinking::Off,
                reasoning_effort: None,
                reasoning_replay: None,
            })
            .map_err(|error| error.to_string())?;
            Ok(SwitchableModel::new(
                initial,
                ModelRoute::new("chatgpt", "gpt-5.4"),
                slot,
                ctx,
            ))
        },
    );

    let host = TaskHost {
        session_dir: session_dir.clone(),
        model: model_factory,
        route: ("chatgpt".into(), "gpt-5.4".into()),
        servers: Vec::new(),
        sandbox: None,
        sandbox_deny_read: Vec::new(),
        allow_unix_sockets: Vec::new(),
        retries: 0,
        context_budget: None,
        auto_compact: false,
        compaction_settings: serde_json::Value::Null,
        compaction_settings_for: None,
        turn_options: TurnOptions::default(),
        turn_timeout: None,
        wording: radian_host::CLI_WORDING,
        protected_branches: Vec::new(),
        allowed_filters: None,
        shell_auto_background: None,
        skills_extra_paths: Vec::new(),
        skills_builtin: true,
        skills_disabled: Vec::new(),
        memory_injection: radian_host::memory::InjectionConfig::default(),
        capabilities: radian_host::SessionCapability::ALL.to_vec(),
        audit: Arc::new(|_journal| Arc::new(NoopAudit) as Arc<dyn radian_permissions::AuditSink>),
        task_events: Arc::new(|_event: &RuntimeEvent| {}),
    };
    let registry = Arc::new(TaskRegistry::new(
        lead.clone(),
        "lead",
        journal,
        queue,
        task_builder(host),
    ));

    // Task 1: no model override — the lead's own default (chatgpt) is used.
    let mut default_request = SpawnRequest::new("t-default");
    default_request.brief = Some("go".into());
    registry
        .spawn(default_request)
        .await
        .expect("the default-route task spawns");
    wait_outcome(&registry, "t-default").await;
    assert_eq!(
        registry.get("t-default").unwrap().last_outcome(),
        Some(TurnOutcome::Completed)
    );

    // Task 2: `model: opencode-go/deepseek-v4.1-flash` — ADR 0030 O11's spawn-argument route.
    let mut routed_request = SpawnRequest::new("t-deepseek");
    routed_request.brief = Some("go".into());
    routed_request.model = Some("opencode-go/deepseek-v4.1-flash".to_owned());
    registry
        .spawn(routed_request)
        .await
        .expect("the routed task spawns");
    wait_outcome(&registry, "t-deepseek").await;
    assert_eq!(
        registry.get("t-deepseek").unwrap().last_outcome(),
        Some(TurnOutcome::Completed)
    );

    // Each request landed on its own mock, with its own auth — never the other's.
    let chatgpt_requests = chatgpt_mock.requests();
    assert_eq!(
        chatgpt_requests.len(),
        1,
        "only the default-route task calls chatgpt"
    );
    assert_eq!(
        chatgpt_requests[0].header("authorization"),
        Some(format!("Bearer {access}").as_str())
    );
    assert_eq!(chatgpt_requests[0].header("originator"), Some("radian"));

    let opencode_requests = opencode_mock.requests();
    assert_eq!(
        opencode_requests.len(),
        1,
        "only the routed task calls opencode-go"
    );
    assert_eq!(
        opencode_requests[0].header("authorization"),
        Some("Bearer sk-task-key"),
        "the task's own api_key, never the lead's chatgpt token"
    );
    assert!(
        !opencode_requests[0]
            .header("authorization")
            .unwrap_or_default()
            .contains(&access),
        "the lead's chatgpt token must never reach the opencode-go mock"
    );

    let _ = std::fs::remove_dir_all(&scratch_dir);
}
