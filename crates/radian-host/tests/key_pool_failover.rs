//! ADR 0026 D2 end to end: a quota `429` rests the key that hit it, and the core's next retry
//! asks the host for the model again and gets the next key. The mock server sees both keys and
//! the turn completes; one notice names both keys.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the same
//! rule `tests/responses_api_key_route.rs` follows).

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_core::{Agent, AgentConfig, NoTools, NoopSink, TurnOptions};
use radian_host::{ModelRoute, RouteContext, SwitchableModel, build_model};
use serde_json::json;

/// A private `RADIAN_HOME` with a two-key `opencode-go` entry pointing at the mock.
fn isolated_home(base_url: &str) -> PathBuf {
    let dir = common::process_dir("radian-key-pool-failover");
    let credentials = dir.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let entry = serde_json::json!({
        "opencode-go": {
            "type": "api_key",
            "key": "sk-first",
            "keys": [{"name": "team", "key": "sk-second"}],
            "env": {"RADIAN_BASE_URL": base_url},
        }
    });
    let auth = credentials.join("auth.json");
    std::fs::write(&auth, serde_json::to_string(&entry).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let dir = std::fs::canonicalize(&dir).unwrap();
    // SAFETY: this binary holds one test, and the variable is set before any credential read.
    unsafe { std::env::set_var("RADIAN_HOME", &dir) };
    dir
}

fn context() -> RouteContext {
    RouteContext {
        extra_body: Default::default(),
        providers: BTreeMap::new(),
        session_id: "key-pool-failover".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    }
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

#[tokio::test]
async fn a_quota_429_switches_to_the_next_key_for_the_retry() {
    let server = MockServer::start(vec![Reply::status(429, "quota"), text_reply("ok")]).await;
    let _home = isolated_home(&server.base_url);
    radian_host::key_pool::reset();

    let ctx = context();
    let args = ctx
        .args_for("opencode-go", "deepseek-v4.1-flash", None)
        .expect("the route resolves");
    let initial = build_model(args).expect("the startup model builds");
    let route = ModelRoute::new("opencode-go", "deepseek-v4.1-flash");
    let slot = radian_runtime::ConfigSlot::new(serde_json::Value::Null);
    let model = SwitchableModel::new(initial, route, slot, ctx);

    let agent = std::sync::Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        retries: 1,
        ..Default::default()
    }));
    let mut chat = agent.open_session().await.expect("a session");
    let outcome = chat
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .expect("the turn finishes");
    assert_eq!(
        outcome.status,
        radian_core::RunStatus::Completed,
        "{:?}",
        outcome.status
    );

    // The first request used the first key and got the 429; the retry used the second.
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "one retry: {}", requests.len());
    assert_eq!(requests[0].header("authorization"), Some("Bearer sk-first"));
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer sk-second"),
        "the rested key is skipped"
    );

    // One notice, naming both keys and never the values.
    let notices = radian_host::key_pool::take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let notice = &notices[0];
    assert!(notice.contains("opencode-go"), "{notice}");
    assert!(notice.contains("\"first\""), "{notice}");
    assert!(notice.contains("\"team\""), "{notice}");
    assert!(notice.contains("rests until"), "{notice}");
    assert!(
        !notice.contains("sk-first") && !notice.contains("sk-second"),
        "{notice}"
    );

    // Nothing key-shaped reached the transcript (the journal replays it under
    // `--features journal-selfcheck`).
    let transcript = serde_json::to_string(chat.transcript()).unwrap();
    assert!(
        !transcript.contains("sk-first") && !transcript.contains("sk-second"),
        "{transcript}"
    );
}
