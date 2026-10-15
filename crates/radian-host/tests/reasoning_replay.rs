//! Reasoning replay resolution (ADR 0014 Q2): for a built-in provider the catalog's
//! `interleaved` field decides, and a `[providers.<id>]` declaration overrides it.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: nothing
//! else in it can race the environment.

use std::collections::BTreeMap;
use std::path::PathBuf;

use radian_host::{
    Protocol, ProviderDecl, ProviderRef, ReasoningReplayOverride, descriptor,
    resolve_reasoning_replay, route_replays_reasoning,
};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "radian-host-reasoning-replay-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cache/catalog")).unwrap();
    dir
}

fn decl(override_: Option<ReasoningReplayOverride>) -> ProviderDecl {
    ProviderDecl {
        kind: "openai-compatible".into(),
        base_url: "https://acme.example/v1".into(),
        reasoning_replay: override_,
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
                "plain": {"limit": {"context": 5}}
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

    // A named provider's declaration overrides the catalog, in both directions.
    let mut on = BTreeMap::new();
    on.insert(
        "acme".to_owned(),
        decl(Some(ReasoningReplayOverride::Field("reasoning".into()))),
    );
    let mut off = BTreeMap::new();
    off.insert(
        "acme".to_owned(),
        decl(Some(ReasoningReplayOverride::Off)),
    );
    assert!(route_replays_reasoning("acme", "m", &on));
    assert!(!route_replays_reasoning("acme", "m", &off));
    // An override of `reasoning` is used verbatim, not the catalog's field.
    let named = ProviderRef::Named(radian_host::NamedProvider {
        name: "acme".into(),
        protocol: Protocol::OpenAiCompatible,
        base_url: "https://acme.example/v1".into(),
        reasoning_replay: Some(ReasoningReplayOverride::Field("reasoning".into())),
    });
    assert_eq!(
        resolve_reasoning_replay(&named, "m").as_deref(),
        Some("reasoning")
    );

    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::remove_var("RADIAN_HOME") };
}
