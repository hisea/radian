//! Reasoning replay resolution (ADR 0014 R5): the catalog's `interleaved` field decides,
//! keyed by provider name and model — a built-in provider, or a named provider that shares a
//! catalog provider's name — from the local cache when it parses, even where it differs from
//! the bundled snapshot (ADR 0018's order), and a `[providers.<id>]` declaration overrides it.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: nothing
//! else in it can race the environment.

mod common;

use std::collections::BTreeMap;

use radian_host::{
    Protocol, ProviderDecl, ProviderRef, ReasoningReplayOverride, descriptor,
    resolve_reasoning_replay, route_replays_reasoning,
};

fn scratch() -> common::TempDir {
    let dir = common::TempDir::named("radian-host-reasoning-replay");
    std::fs::create_dir_all(dir.join("cache/catalog")).unwrap();
    dir
}

fn decl(override_: Option<ReasoningReplayOverride>) -> ProviderDecl {
    ProviderDecl {
        kind: "openai-compatible".into(),
        base_url: "https://acme.example/v1".into(),
        reasoning_replay: override_,
        ..Default::default()
    }
}

#[test]
fn the_catalog_and_a_named_override_decide_reasoning_replay() {
    let home = scratch();
    std::fs::write(
        home.join("cache/catalog/models.json"),
        r#"{
            "opencode-go": {"models": {
                "deepseek-v4.1-flash": {"limit": {"context": 1000000}, "interleaved": {"field": "reasoning_content"}},
                "glm-5.2": {"limit": {"context": 5}, "interleaved": true},
                "alt": {"limit": {"context": 5}, "interleaved": {"field": "reasoning"}},
                "plain": {"limit": {"context": 5}}
            }},
            "opencode": {"models": {
                "deepseek-v4.1-flash": {"limit": {"context": 5}, "interleaved": {"field": "reasoning"}}
            }}
        }"#,
    )
    .unwrap();
    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

    // A built-in provider: the catalog decides.
    let builtin = ProviderRef::BuiltIn(descriptor("opencode-go").unwrap());
    assert_eq!(
        resolve_reasoning_replay(&builtin, "deepseek-v4.1-flash").as_deref(),
        Some("reasoning_content")
    );
    assert_eq!(resolve_reasoning_replay(&builtin, "plain"), None);
    // The cache wins over the bundled snapshot: the snapshot marks `glm-5.2` with
    // `{"field": "reasoning_content"}`, and this cache says a bare `true`, which is not
    // replayed. A model the snapshot does not carry is read from the cache too.
    assert_eq!(resolve_reasoning_replay(&builtin, "glm-5.2"), None);
    assert_eq!(
        resolve_reasoning_replay(&builtin, "alt").as_deref(),
        Some("reasoning")
    );
    assert!(route_replays_reasoning(
        "opencode-go",
        "deepseek-v4.1-flash",
        &BTreeMap::new()
    ));
    assert!(!route_replays_reasoning(
        "opencode-go",
        "plain",
        &BTreeMap::new()
    ));

    // A named provider with no override is read from the catalog by its **name** too (ADR 0026
    // D10: `None` falls through to the catalog). `opencode` is a catalog provider, not a
    // built-in descriptor, so a `[providers.opencode]` route sees this cache's entry (the
    // snapshot marks the same model `reasoning_content`).
    let named_no_override = ProviderRef::Named(radian_host::NamedProvider {
        name: "opencode".into(),
        protocol: Protocol::OpenAiCompatible,
        base_url: "https://acme.example/v1".into(),
        reasoning_replay: None,
        rules: Vec::new(),
        prefer: None,
        base_urls: BTreeMap::new(),
        model_list: None,
    });
    assert_eq!(
        resolve_reasoning_replay(&named_no_override, "deepseek-v4.1-flash").as_deref(),
        Some("reasoning")
    );

    // A named provider's declaration overrides the catalog, in both directions.
    let mut on = BTreeMap::new();
    on.insert(
        "acme".to_owned(),
        decl(Some(ReasoningReplayOverride::Field("reasoning".into()))),
    );
    let mut off = BTreeMap::new();
    off.insert("acme".to_owned(), decl(Some(ReasoningReplayOverride::Off)));
    assert!(route_replays_reasoning("acme", "m", &on));
    assert!(!route_replays_reasoning("acme", "m", &off));
    // An override of `reasoning` is used verbatim, not the catalog's field.
    let named = ProviderRef::Named(radian_host::NamedProvider {
        name: "acme".into(),
        protocol: Protocol::OpenAiCompatible,
        base_url: "https://acme.example/v1".into(),
        reasoning_replay: Some(ReasoningReplayOverride::Field("reasoning".into())),
        rules: Vec::new(),
        prefer: None,
        base_urls: BTreeMap::new(),
        model_list: None,
    });
    assert_eq!(
        resolve_reasoning_replay(&named, "m").as_deref(),
        Some("reasoning")
    );

    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::remove_var("RADIAN_HOME") };
}
