//! The Rust reader must agree with `docs/schema/check-config-fixtures.sh`'s `EXPECT` table
//! (ADR 0008 §7). The table is duplicated here on purpose: if the two ever disagree, the
//! reader is wrong, and this test fails rather than the checker being bent to match.
//!
//! `readonly_or_refuse` is `readonly` here: this reader interprets a higher-version file (it
//! must never rewrite it, but reading is allowed).

use std::path::PathBuf;

use radian_host::config;

fn fixture(name: &str) -> (PathBuf, String) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("docs/schema/config").join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    (path, text)
}

fn outcome(name: &str) -> &'static str {
    let (path, text) = fixture(name);
    match config::parse(&path, &text) {
        Ok(config) if config.read_only => "readonly",
        Ok(_) => "accept",
        Err(_) => "reject",
    }
}

/// The same table as `check-config-fixtures.sh` (hand-synced, like the fixture README).
#[test]
fn the_reader_matches_the_reference_checker() {
    let cases = [
        ("minimal.toml", "accept"),
        ("v1-unknown-fields.toml", "accept"),
        ("schema-version-newer.toml", "readonly"),
        ("schema-version-lower.toml", "reject"),
        ("plaintext-secret.toml", "reject"),
        ("heuristic-secret.toml", "reject"),
        ("plaintext-plain.toml", "reject"),
    ];
    for (name, expected) in cases {
        assert_eq!(outcome(name), expected, "{name}");
    }
}

/// `v1-unknown-fields.toml` is accepted *and* the unknown fields are reported (ignored, not
/// lost — the reader never writes, so §5.4's write-back rule cannot be violated).
#[test]
fn the_unknown_fields_are_ignored_but_reported() {
    let (path, text) = fixture("v1-unknown-fields.toml");
    let config = config::parse(&path, &text).unwrap();
    assert!(config.skipped.contains(&"future_top_level".to_string()));
    assert!(config.skipped.contains(&"model.future_option".to_string()));
    assert!(config.skipped.contains(&"tui.theme".to_string()));
}

/// The minimal fixture carries the known `tui.mouse` preference (`false`): the reader keeps
/// the value, so the TUI can honor it (ADR 0023 U2, plan B).
#[test]
fn the_tui_mouse_preference_is_read_from_the_minimal_fixture() {
    let (path, text) = fixture("minimal.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.tui.mouse, Some(false));
}

/// The refusal messages never echo a value that could be a secret.
#[test]
fn the_refusal_messages_never_echo_a_value() {
    for name in [
        "plaintext-secret.toml",
        "heuristic-secret.toml",
        "plaintext-plain.toml",
    ] {
        let (path, text) = fixture(name);
        let error = config::parse(&path, &text).unwrap_err().to_string();
        for needle in ["sk-live", "sk-heuristic", "super-secret"] {
            assert!(!error.contains(needle), "{name}: {error}");
        }
    }
}
