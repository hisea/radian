//! The **switch** path carries the reasoning replay (ADR 0014 R4/Q2): a `/model`
//! switch builds its model arguments with `RouteContext::args_for`, so that one call must
//! resolve `reasoning_replay` from the catalog's `interleaved` field or a `[providers.<name>]`
//! override exactly as the startup path does. Replacing that resolution with `None` used to
//! leave every `radian-host` and `radian-tui` test green.
//!
//! The resolver the hosts' budget closures call (`route_replays_reasoning`) is asserted next
//! to it, so the two paths cannot drift: the estimate counts reasoning exactly when the request
//! carries it (ADR 0014 R6).
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: nothing
//! else in it can race the environment.

use std::collections::BTreeMap;
use std::path::Path;

mod common;

use radian_host::{ProviderDecl, ReasoningReplayOverride, RouteContext, route_replays_reasoning};

/// A private `RADIAN_HOME` for this test binary: the catalog cache and the credential store
/// live under it, so the real `~/.radian` is never read or written.
fn scratch() -> common::TempDir {
    let dir = common::TempDir::named("radian-host-switch-replay");
    std::fs::create_dir_all(dir.join("cache/catalog")).unwrap();
    std::fs::create_dir_all(dir.join("credentials")).unwrap();
    #[cfg(unix)]
    {
        // 0700: the store warns about a group/world-readable directory, and this is a
        // credential directory.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("credentials"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    dir
}

/// The catalog cache for the fixture providers: `opencode-go` is built in, `acme` and `quiet`
/// are config-declared, and every model that carries `interleaved` will be overridden below.
fn write_catalog(home: &Path) {
    std::fs::write(
        home.join("cache/catalog/models.json"),
        r#"{
            "opencode-go": {"models": {
                "deepseek-v4.1-flash": {"limit": {"context": 1000000}, "interleaved": {"field": "reasoning_content"}},
                "plain": {"limit": {"context": 5}}
            }},
            "acme": {"models": {
                "m": {"limit": {"context": 1000}, "interleaved": {"field": "reasoning_content"}}
            }},
            "quiet": {"models": {
                "m": {"limit": {"context": 1000}, "interleaved": {"field": "reasoning_content"}}
            }}
        }"#,
    )
    .unwrap();
}

/// A usable `api_key` entry for each name: `args_for` refuses a route with no credential
/// (ADR 0026 D6), and the store refuses a group/world-readable file.
fn write_auth(home: &Path, names: &[&str]) {
    let entries: serde_json::Map<String, serde_json::Value> = names
        .iter()
        .map(|name| {
            (
                (*name).to_owned(),
                serde_json::json!({"type": "api_key", "key": format!("test-key-{name}")}),
            )
        })
        .collect();
    let path = home.join("credentials/auth.json");
    std::fs::write(&path, serde_json::Value::Object(entries).to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn decl(base_url: &str, override_: Option<ReasoningReplayOverride>) -> ProviderDecl {
    ProviderDecl {
        kind: "openai-compatible".into(),
        base_url: base_url.into(),
        reasoning_replay: override_,
    }
}

fn context() -> RouteContext {
    RouteContext {
        providers: BTreeMap::from([
            (
                "acme".to_owned(),
                decl(
                    "https://acme.example/v1",
                    Some(ReasoningReplayOverride::Field("reasoning".into())),
                ),
            ),
            (
                "quiet".to_owned(),
                decl(
                    "https://quiet.example/v1",
                    Some(ReasoningReplayOverride::Off),
                ),
            ),
        ]),
        session_id: "switch-replay".to_owned(),
        user_headers: Vec::new(),
        include_usage: None,
        max_tokens: 4096,
        thinking: "medium".to_owned(),
        thinking_budget: None,
    }
}

#[test]
fn the_switch_path_carries_the_reasoning_replay() {
    let home = scratch();
    write_catalog(&home);
    write_auth(&home, &["opencode-go", "acme", "quiet"]);
    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

    let ctx = context();
    // The route the model would replay under, as `args_for` resolved it, and the answer the
    // budget closure's resolver gives for the same switch. They must agree every time.
    let check = |name: &str, model: &str, expected: Option<&str>| {
        let args = ctx.args_for(name, model).unwrap();
        assert_eq!(args.reasoning_replay.as_deref(), expected, "{name}/{model}");
        assert_eq!(
            route_replays_reasoning(name, model, &ctx.providers),
            expected.is_some(),
            "the budget's answer must match the request's field for {name}/{model}"
        );
    };

    // A built-in provider: the catalog's `interleaved` field decides, and a model without one
    // replays nothing.
    check(
        "opencode-go",
        "deepseek-v4.1-flash",
        Some("reasoning_content"),
    );
    check("opencode-go", "plain", None);

    // A `[providers.<name>]` override wins over the catalog, in both directions: `acme` names
    // another field even though its catalog entry says `reasoning_content`, and `quiet` turns
    // replay off for a model whose catalog entry asks for it.
    check("acme", "m", Some("reasoning"));
    check("quiet", "m", None);

    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::remove_var("RADIAN_HOME") };
}
