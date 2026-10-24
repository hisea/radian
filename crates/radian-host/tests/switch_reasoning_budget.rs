//! The **switched** route's budget counts reasoning exactly when that route replays it
//! (ADR 0014 R6). `route_replays_reasoning` is the function both hosts' `budget_for`
//! closures call (`crates/radian-tui/src/main.rs`, `crates/radian-exec/src/lib.rs`); this test
//! builds the budget the same way the closure does, which is the level reachable without a
//! terminal. Forcing that resolver to `false` used to leave every `radian-host` and
//! `radian-tui` test green.
//!
//! `RADIAN_HOME` is process-wide, so this binary deliberately holds exactly one test: nothing
//! else in it can race the environment.

use std::collections::BTreeMap;
use std::path::Path;

use radian_core::ContextBudget;
mod common;

use radian_host::{ProviderDecl, ReasoningReplayOverride, route_replays_reasoning};

/// A private `RADIAN_HOME` for this test binary: the catalog cache lives under it, so the real
/// `~/.radian` is never read.
fn scratch() -> common::TempDir {
    let dir = common::TempDir::named("radian-host-switch-budget");
    std::fs::create_dir_all(dir.join("cache/catalog")).unwrap();
    dir
}

/// The catalog cache for the fixture: `opencode-go` is built in, `acme` and `quiet` are
/// config-declared, and every entry that carries `interleaved` is overridden below.
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

fn decl(base_url: &str, override_: Option<ReasoningReplayOverride>) -> ProviderDecl {
    ProviderDecl {
        kind: "openai-compatible".into(),
        base_url: base_url.into(),
        reasoning_replay: override_,
    }
}

#[test]
fn a_switched_route_counts_reasoning_exactly_when_it_replays_it() {
    let home = scratch();
    write_catalog(&home);
    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::set_var("RADIAN_HOME", &home) };

    let providers: BTreeMap<String, ProviderDecl> = BTreeMap::from([
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
    ]);
    // The hosts' `budget_for` closure in one line: the window's tokens, with reasoning counted
    // exactly when this route replays it.
    let budget = |provider: &str, model: &str| {
        ContextBudget::new(1_000)
            .with_reasoning(route_replays_reasoning(provider, model, &providers))
    };

    // A built-in provider: the catalog's `interleaved` field decides.
    assert!(
        budget("opencode-go", "deepseek-v4.1-flash").count_reasoning,
        "a switched-to replaying model must count its reasoning"
    );
    assert!(
        !budget("opencode-go", "plain").count_reasoning,
        "a model that replays nothing must not count reasoning"
    );

    // A `[providers.<name>]` override wins over the catalog, in both directions.
    assert!(
        budget("acme", "m").count_reasoning,
        "the override turns replay on for a model the catalog asks for"
    );
    assert!(
        !budget("quiet", "m").count_reasoning,
        "reasoning_replay = false wins over the catalog"
    );

    // SAFETY: this test binary has exactly one test.
    unsafe { std::env::remove_var("RADIAN_HOME") };
}
