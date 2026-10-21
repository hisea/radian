//! ADR 0026 end to end, through the host's route resolution (`RouteContext::args_for`,
//! the exact function a `/model` switch and the startup path call): a built-in descriptor's
//! `@ai-sdk/openai` model resolves to the **Responses** protocol, and with an API key (not OAuth)
//! to the plain Responses route, while an `npm` radian has no adapter for is refused by name.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the same
//! rule `tests/chatgpt_switch.rs` and `tests/switch_reasoning_replay.rs` follow).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, message_item, output_item_done,
};
use radian_core::{CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink};
use radian_host::{Provider, RouteContext, build_model};
use serde_json::Value;

/// A private `RADIAN_HOME` (no catalog cache: the bundled snapshot answers) with a usable
/// `opencode-go` credential whose `RADIAN_BASE_URL` points at the mock.
fn isolated_home(base_url: &str) -> PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "radian-responses-api-key-route-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
                "key": "sk-opencode-test",
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
        // SAFETY: `get_or_init` runs this once, before any catalog or credential read in this
        // binary (this is its only test).
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
}

fn context() -> RouteContext {
    RouteContext {
        providers: BTreeMap::new(),
        session_id: "responses-api-key".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    }
}

#[tokio::test]
async fn a_catalog_responses_model_on_a_built_in_descriptor_uses_the_api_key_route() {
    let server = MockServer::start(vec![Reply::sse(&[
        output_item_done(0, message_item("msg_1", "route-ok")),
        completed(None),
    ])])
    .await;
    let _home = isolated_home(&server.base_url);
    let ctx = context();

    // `muse-spark-1.3-contributor` is `@ai-sdk/openai` in the bundled snapshot, so the route is
    // Responses even though the built-in table now also says so; the endpoint is the credential's
    // override (the mock), and the credential is an API key, not OAuth.
    let args = ctx
        .args_for("opencode-go", "muse-spark-1.3-contributor")
        .expect("the route resolves");
    assert_eq!(args.provider, Provider::ChatGpt);
    assert_eq!(args.base_url, server.base_url);
    assert_eq!(args.api_key.as_deref(), Some("sk-opencode-test"));
    assert!(
        !args.chatgpt_oauth,
        "an API key is not the ChatGPT OAuth route"
    );
    assert!(
        args.headers
            .iter()
            .any(|(name, _)| name == "x-opencode-session"),
        "the descriptor's own headers: {:?}",
        args.headers
    );

    // The whole turn goes out with the API key and no Codex header.
    let model = build_model(args).expect("the API-key Responses model builds");
    let response = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers");
    assert_eq!(
        response.content,
        vec![radian_core::ContentPart::text("route-ok")]
    );
    let recorded = &server.requests()[0];
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-opencode-test")
    );
    for name in ["chatgpt-account-id", "openai-beta", "originator"] {
        assert!(recorded.header(name).is_none(), "{name} must not be sent");
    }

    // An `npm` radian has no adapter for is refused by name, never silently sent as chat.
    let error = match ctx.args_for("opencode-go", "gemini-4-argon-contributor") {
        Err(error) => error,
        Ok(_) => panic!("a Google model must be refused"),
    };
    assert_eq!(
        error,
        "radian does not support this model's API (@ai-sdk/google)"
    );

    // A plain chat model on the same descriptor still resolves to the chat protocol.
    let chat = ctx
        .args_for("opencode-go", "deepseek-v4.1-flash")
        .expect("the chat route resolves");
    assert_eq!(chat.provider, Provider::OpenAiCompatible);
}
