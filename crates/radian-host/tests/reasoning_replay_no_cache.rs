//! Reasoning replay with **no catalog cache** (ADR 0014 R5, ADR 0018): the bundled snapshot is
//! the last fallback, exactly as it is for the wire protocol and image support. A fresh
//! `RADIAN_HOME` — a new user's first run, or every Terminal-Bench container, which runs with
//! `--offline-catalog` and a new home — has no cache, so this lookup used to return `None` and
//! no assistant message carried `reasoning_content` even though the snapshot declares
//! `interleaved: {"field": "reasoning_content"}` for the model.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: nothing
//! else in it can race the environment.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use radian_host::{
    Protocol, ProviderDecl, ProviderRef, ReasoningReplayOverride, RouteContext, descriptor,
    resolve_reasoning_replay, route_replays_reasoning,
};

/// A home for this binary with **no** `cache/catalog/models.json`: a first run.
fn scratch() -> common::TempDir {
    let dir = common::TempDir::named("radian-host-reasoning-replay-no-cache");
    assert!(
        !dir.join("cache/catalog/models.json").exists(),
        "the fixture must have no catalog cache"
    );
    dir
}

/// The `api_key` entries, so the switch path's `args_for` does not refuse the route
/// (ADR 0026 D6); the store refuses a group/world-readable file.
fn write_auth(home: &Path, names: &[&str]) {
    std::fs::create_dir_all(home.join("credentials")).unwrap();
    let path = home.join("credentials/auth.json");
    let entries: Vec<String> = names
        .iter()
        .map(|name| format!(r#""{name}": {{"type": "api_key", "key": "test-key"}}"#))
        .collect();
    std::fs::write(&path, format!("{{{}}}", entries.join(","))).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            home.join("credentials"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

#[test]
fn with_no_cache_the_bundled_snapshot_decides_reasoning_replay() {
    let home = scratch();
    write_auth(&home, &["opencode-go", "opencode"]);
    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

    let builtin = ProviderRef::BuiltIn(descriptor("opencode-go").unwrap());
    // The snapshot declares `interleaved: {"field": "reasoning_content"}` for this model.
    assert_eq!(
        resolve_reasoning_replay(&builtin, "deepseek-v4.1-flash").as_deref(),
        Some("reasoning_content"),
        "with no cache, the bundled snapshot still decides replay"
    );
    // A model the snapshot does not mark replays nothing.
    assert_eq!(resolve_reasoning_replay(&builtin, "gpt-5.6-luna"), None);
    // A bare `true` in the snapshot is unsupported, not a field (ADR 0014 R5).
    assert_eq!(
        resolve_reasoning_replay(&builtin, "claude-sonnet-4-5"),
        None
    );

    // ADR 0014 R6: the budget resolver counts reasoning exactly when the request carries it.
    assert!(route_replays_reasoning(
        "opencode-go",
        "deepseek-v4.1-flash",
        &BTreeMap::new()
    ));
    assert!(!route_replays_reasoning(
        "opencode-go",
        "gpt-5.6-luna",
        &BTreeMap::new()
    ));

    // ADR 0014 R6: the budget's answer is exactly what the request carries when a `/model`
    // switch builds the route (`args_for`, the same call the startup path makes).
    let ctx = RouteContext {
        providers: BTreeMap::new(),
        session_id: "no-cache".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    };
    for (model, expected) in [
        ("deepseek-v4.1-flash", Some("reasoning_content")),
        ("gpt-5.6-luna", None),
    ] {
        let args = ctx.args_for("opencode-go", model, None).unwrap();
        assert_eq!(args.reasoning_replay.as_deref(), expected, "{model}");
        assert_eq!(
            route_replays_reasoning("opencode-go", model, &ctx.providers),
            expected.is_some(),
            "the budget's answer must match the request's field for {model}"
        );
    }

    // A named provider with no `reasoning_replay` resolves through the catalog by its **name**
    // too, not only a built-in (ADR 0026 D10: `None` falls through to the catalog). A custom
    // endpoint may be named after a catalog provider (`opencode` is not a built-in descriptor),
    // and with no cache the bundled snapshot answers for it exactly as for a built-in.
    let named_ref = |name: &str| {
        ProviderRef::Named(radian_host::NamedProvider {
            name: name.into(),
            protocol: Protocol::OpenAiCompatible,
            base_url: "https://acme.example/v1".into(),
            reasoning_replay: None,
        })
    };
    assert_eq!(
        resolve_reasoning_replay(&named_ref("opencode"), "deepseek-v4.1-flash").as_deref(),
        Some("reasoning_content"),
        "a named provider with no override reads the snapshot by its name"
    );
    assert_eq!(
        resolve_reasoning_replay(&named_ref("opencode"), "gpt-5.6-luna"),
        None
    );
    assert_eq!(
        resolve_reasoning_replay(&named_ref("acme"), "deepseek-v4.1-flash"),
        None,
        "a name no catalog provider shares replays nothing"
    );

    // The same route through the config map and the request builder: what the budget counts is
    // what the request carries (ADR 0014 R6).
    let mut providers = BTreeMap::new();
    providers.insert(
        "opencode".to_owned(),
        ProviderDecl {
            kind: "openai-compatible".into(),
            base_url: "https://acme.example/v1".into(),
            reasoning_replay: None,
        },
    );
    assert!(route_replays_reasoning(
        "opencode",
        "deepseek-v4.1-flash",
        &providers
    ));
    assert!(!route_replays_reasoning(
        "opencode",
        "gpt-5.6-luna",
        &providers
    ));
    let named_ctx = RouteContext {
        providers: providers.clone(),
        session_id: "no-cache-named".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    };
    assert_eq!(
        named_ctx
            .args_for("opencode", "deepseek-v4.1-flash", None)
            .unwrap()
            .reasoning_replay
            .as_deref(),
        Some("reasoning_content")
    );

    // The `[providers.<id>]` override is still first, ahead of the snapshot, in both directions.
    // The name matches a built-in so the snapshot has an entry for the same (provider, model):
    // without an override the answer would be the snapshot's `reasoning_content`.
    let named = |override_: ReasoningReplayOverride| {
        ProviderRef::Named(radian_host::NamedProvider {
            name: "opencode-go".into(),
            protocol: Protocol::OpenAiCompatible,
            base_url: "https://acme.example/v1".into(),
            reasoning_replay: Some(override_),
        })
    };
    assert_eq!(
        resolve_reasoning_replay(
            &named(ReasoningReplayOverride::Field("reasoning".into())),
            "deepseek-v4.1-flash",
        )
        .as_deref(),
        Some("reasoning")
    );
    assert_eq!(
        resolve_reasoning_replay(&named(ReasoningReplayOverride::Off), "deepseek-v4.1-flash"),
        None
    );

    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::remove_var("RADIAN_HOME") };
}
