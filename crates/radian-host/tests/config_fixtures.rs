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
        ("tui-theme-colors.toml", "accept"),
        ("providers.toml", "accept"),
        ("skills-extra-paths.toml", "accept"),
        ("v1-unknown-fields.toml", "accept"),
        ("schema-version-newer.toml", "readonly"),
        ("schema-version-lower.toml", "reject"),
        ("api-key-removed.toml", "reject"),
        ("plaintext-secret.toml", "reject"),
        ("heuristic-secret.toml", "reject"),
        ("plaintext-plain.toml", "reject"),
    ];
    for (name, expected) in cases {
        assert_eq!(outcome(name), expected, "{name}");
    }
}

/// `skills-extra-paths.toml` (ADR 0025 slice 4): the reader keeps the strings verbatim; the
/// host resolves them (`~`, relative to the config file). The reader must not touch the
/// filesystem.
#[test]
fn skills_extra_paths_are_carried_raw() {
    let (path, text) = fixture("skills-extra-paths.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(
        config.skills.extra_paths,
        vec![
            "~/.claude/skills".to_owned(),
            "./team-skills".to_owned(),
            "/opt/shared/skills".to_owned(),
        ]
    );
    assert!(
        !config.skipped.iter().any(|key| key.starts_with("skills")),
        "{:?}",
        config.skipped
    );
}

/// `tui-display.toml` (ADR 0028 R2/R3/R4/R8): the display preferences are carried, including
/// `highlight` (R2) and `mermaid` (R8).
#[test]
fn tui_display_preferences_are_carried() {
    let (path, text) = fixture("tui-display.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.tui.display_reasoning.as_deref(), Some("collapsed"));
    assert_eq!(config.tui.display_tool_output.as_deref(), Some("errors"));
    assert_eq!(config.tui.display_tool_preview_lines, Some(8));
    assert_eq!(config.tui.display_mermaid, Some(true));
    assert_eq!(config.tui.display_highlight, Some(false));
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

/// The `providers.toml` fixture (ADR 0026 D10) carries named providers; the names are the
/// `auth.json` key slots, so `--provider corp` and `/model corp/…` resolve them.
#[test]
fn the_providers_fixture_carries_named_providers() {
    let (path, text) = fixture("providers.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.model.provider.as_deref(), Some("corp"));
    assert_eq!(config.providers.len(), 2);
    assert_eq!(config.providers["corp"].kind, "openai-compatible");
    assert_eq!(
        config.providers["corp"].base_url,
        "https://llm.example.com/v1"
    );
    assert_eq!(config.providers["relay"].kind, "anthropic");
    assert!(config.skipped.is_empty(), "{:?}", config.skipped);
}

/// The `tui-theme-colors.toml` fixture (ADR 0029) carries a theme-file name and raw
/// `[tui.colors]` entries: the reader validates nothing beyond the TOML shape, the TUI
/// resolves names and colors and reports the misses as startup warnings.
#[test]
fn the_tui_theme_colors_fixture_is_carried_raw() {
    let (path, text) = fixture("tui-theme-colors.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.tui.theme.as_deref(), Some("kanagawa-wave"));
    assert!(config.skipped.is_empty());
    let get = |key: &str| {
        config
            .tui
            .colors
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
            .unwrap()
    };
    assert_eq!(
        get("user").fg,
        Some(config::TuiColorValue::Text("#0af".to_owned()))
    );
    assert_eq!(get("tool_call").fg, Some(config::TuiColorValue::Index(39)));
    let notice = get("notice");
    assert_eq!(
        notice.fg,
        Some(config::TuiColorValue::Text("oklch(62% 0.1 200)".to_owned()))
    );
    assert_eq!(notice.bold, Some(true));
    // A pi name is carried like any other key; the TUI decides where it lands.
    assert!(get("mdHeading").underline == Some(false));
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
