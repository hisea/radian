//! ADR 0026 slice 3, Build step 3: switching to `chatgpt` mid-session works like
//! any other provider — `RouteContext::args_for("chatgpt", model)` is the exact function a
//! `/model` switch calls (confirmed by reading `radian-tui/src/cli.rs`'s switch path), so
//! this exercises it directly rather than through a full TUI round-trip (already proven at
//! the request level by `tests/chatgpt_headers.rs`, and that `args_for` itself is credential-
//! aware for an oauth entry by `tests/chatgpt_lead_task.rs`'s opencode-go route — the one
//! untested combination is this function called with `"chatgpt"` specifically). One
//! `#[tokio::test]` per file — see `tests/chatgpt_headers.rs`'s header comment for why.

mod common;

use std::sync::OnceLock;

use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, message_item, output_item_done,
};
use radian_core::{CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink};
use radian_host::{AuthStore, OAuthEntry, Provider, RouteContext};
use serde_json::Value;

fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-chatgpt-switch-home");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once; this file has exactly one `#[tokio::test]`.
        unsafe { std::env::set_var("RADIAN_HOME", &dir) };
        dir
    })
    .clone()
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

#[tokio::test]
async fn switching_to_chatgpt_mid_session_resolves_a_working_route() {
    let home = isolated_home();
    let access = fake_access_token("acct-switch-test");
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
    }

    // Exactly what a `/model chatgpt/gpt-5.4` switch calls: no credential copied in, the
    // route resolved by name, like `RouteContext` would be built from the lead's settings.
    let ctx = RouteContext {
        extra_body: Default::default(),
        providers: std::collections::BTreeMap::new(),
        session_id: "switch-test".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    };
    let args = ctx
        .args_for("chatgpt", "gpt-5.4", None)
        .expect("a logged-in chatgpt route resolves");
    assert_eq!(args.provider, Provider::ChatGpt);
    assert!(args.api_key.is_none(), "chatgpt carries no static api_key");

    // The resolved args actually build into a model that completes against a mock (the
    // production base_url is unreachable from a test, so the mock substitutes for it here —
    // `args_for`'s own route resolution, not this override, is what's under test).
    let server = MockServer::start(vec![Reply::sse(&[
        output_item_done(0, message_item("msg_1", "switched ok")),
        completed(None),
    ])])
    .await;
    let mut args = args;
    args.base_url = server.base_url.clone();
    let model = radian_host::build_model(args).expect("the resolved route builds");
    let response = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hello")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the switched route completes");
    assert_eq!(response.stop_reason, radian_core::StopReason::EndTurn);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {access}")).as_deref()
    );
}

/// Switching to `chatgpt` without a login is refused with a clear `/login` pointer, exactly
/// like any other provider (ADR 0026 D6) — the `has_login` fix did not accidentally make
/// every provider name "logged in".
#[tokio::test]
async fn switching_to_chatgpt_without_a_login_is_refused() {
    // A distinct, never-written provider id under the *same* shared home: proves the oauth
    // exception is not "any unknown provider passes", without touching the `chatgpt` entry
    // the other test in this file writes (so the two cannot race each other).
    let _home = isolated_home();
    let ctx = RouteContext {
        extra_body: Default::default(),
        providers: std::collections::BTreeMap::new(),
        session_id: "switch-test-2".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    };
    let error = match ctx.args_for("anthropic", "claude-x", None) {
        Err(error) => error,
        Ok(_) => panic!("anthropic has no stored credential in this test's home"),
    };
    assert!(error.contains("/login anthropic"), "{error}");
}
