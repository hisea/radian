//! ADR 0018 §2: the switched-route output reservation. `RouteContext::output_for` answers the
//! budget query a `/model` switch and an `exec` resume use, and `RouteContext::args_for`
//! builds the request; both must count an explicit output cap from `RADIAN_EXTRA_BODY`, not
//! the catalog cap it replaces.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test (the same
//! rule `tests/named_model_routing.rs` follows).

mod common;

use std::collections::BTreeMap;
use std::sync::OnceLock;

use radian_host::{ProviderDecl, RouteContext};
use serde_json::{Value, json};

/// A private `RADIAN_HOME` with one usable `magpie` credential, so `args_for` can resolve a
/// key without reading the real `~/.radian`.
fn isolated_home() -> std::path::PathBuf {
    static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = common::process_dir("radian-route-context-output");
        let credentials = dir.join("credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let entry = json!({
            "magpie": {"type": "api_key", "key": "sk-magpie"}
        });
        let auth = credentials.join("auth.json");
        std::fs::write(&auth, serde_json::to_string(&entry).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let path = std::fs::canonicalize(&dir).unwrap();
        // SAFETY: `get_or_init` runs this once, before any credential read in this binary
        // (this is its only test).
        unsafe { std::env::set_var("RADIAN_HOME", &path) };
        path
    })
    .clone()
}

fn context(base_url: &str, extra_body: Value) -> RouteContext {
    let mut providers = BTreeMap::new();
    providers.insert(
        "magpie".to_owned(),
        ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: base_url.to_owned(),
            ..Default::default()
        },
    );
    RouteContext {
        providers,
        session_id: "route-context-output".to_owned(),
        user_headers: Vec::new(),
        extra_body: extra_body.as_object().unwrap().clone(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "off".to_owned(),
        thinking_budget: None,
    }
}

#[test]
fn a_switched_route_counts_an_explicit_output_cap() {
    let _home = isolated_home();

    // C = 128000, O = 384000: the catalog chat cap is 19200, but the body's 32000 wins on the
    // wire, so `output_for` and the built request both report 32000.
    let ctx = context("https://magpie.example/v1", json!({"max_tokens": 32000}));
    assert_eq!(
        ctx.output_for("magpie", "chat-model")
            .resolve(Some(128_000), Some(384_000)),
        Some(32_000)
    );
    let args = ctx
        .args_for("magpie", "chat-model", Some("sk-magpie"))
        .expect("the chat route builds");
    assert_eq!(args.max_output_tokens, Some(32_000));

    // `api.openai.com`'s chat route resolves to `RouteOutput::None`, so the sibling spelling
    // is the only explicit cap a body can carry there.
    let ctx = context(
        "https://api.openai.com/v1",
        json!({"max_completion_tokens": 32000}),
    );
    assert_eq!(
        ctx.output_for("magpie", "chat-model")
            .resolve(Some(128_000), Some(384_000)),
        Some(32_000)
    );
    let args = ctx
        .args_for("magpie", "chat-model", Some("sk-magpie"))
        .expect("the chat route builds");
    assert_eq!(args.max_output_tokens, Some(32_000));
}
