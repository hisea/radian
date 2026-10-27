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
        ("tui-footer-command.toml", "accept"),
        ("providers.toml", "accept"),
        ("providers-model-routing.toml", "accept"),
        ("model-retries.toml", "accept"),
        ("skills-extra-paths.toml", "accept"),
        ("skills-builtin.toml", "accept"),
        ("shell-auto-background.toml", "accept"),
        ("model-smol.toml", "accept"),
        ("mcp-servers.toml", "accept"),
        ("mcp-mcp-servers.toml", "accept"),
        ("memory-injection.toml", "accept"),
        ("integrations-herdr.toml", "accept"),
        ("control.toml", "accept"),
        ("hooks.toml", "accept"),
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

/// `skills-builtin.toml` (ADR 0025): `builtin` and `disabled` are carried.
#[test]
fn skills_builtin_and_disabled_are_carried() {
    let (path, text) = fixture("skills-builtin.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.skills.builtin, Some(false));
    assert_eq!(config.skills.disabled, vec!["theme-import".to_owned()]);
    assert!(
        !config.skipped.iter().any(|key| key.starts_with("skills")),
        "{:?}",
        config.skipped
    );
}

/// `model-retries.toml` (ADR 0017): `[model] retries` is carried, not skipped.
#[test]
fn model_retries_is_carried() {
    let (path, text) = fixture("model-retries.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.model.retries, Some(3));
    assert!(
        !config.skipped.iter().any(|key| key == "model.retries"),
        "{:?}",
        config.skipped
    );
}

/// `shell-auto-background.toml`: the removed `[shell] auto_background_secs` is accepted
/// but warned once and ignored, never an error.
#[test]
fn the_removed_shell_key_warns_once_and_is_ignored() {
    let (path, text) = fixture("shell-auto-background.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(
        config
            .skipped
            .iter()
            .filter(|key| key.as_str() == "shell.auto_background_secs")
            .count(),
        1,
        "{:?}",
        config.skipped
    );
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

/// `hooks.toml` (ADR 0045 slice A): the three events parse with matchers and commands;
/// the non-subset entries (`http`, `async`, unknown event) are skipped, never refused.
#[test]
fn the_hooks_fixture_parses_the_subset_and_skips_the_rest() {
    let (path, text) = fixture("hooks.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.hooks.pre_tool_use.len(), 1);
    assert_eq!(
        config.hooks.pre_tool_use[0].matcher.as_deref(),
        Some("shell|write")
    );
    assert_eq!(config.hooks.pre_tool_use[0].hooks.len(), 1);
    assert_eq!(config.hooks.pre_tool_use[0].hooks[0].timeout_secs, Some(10));
    assert_eq!(config.hooks.post_tool_use.len(), 1);
    assert_eq!(config.hooks.stop.len(), 1);
    assert_eq!(config.hooks.stop[0].hooks[0].timeout_secs, Some(30));
    assert_eq!(
        config.hooks.intent_stop,
        Some(false),
        "the fixture pins the kill switch off"
    );
    for key in [
        "hooks.PostToolUse[1]",
        "hooks.PreToolUse[1]",
        "hooks.SessionStart",
    ] {
        assert!(
            config.skipped.contains(&key.to_string()),
            "{key}: {:?}",
            config.skipped
        );
    }
}

/// `control.toml` (ADR 0043): `[control] enabled` is carried, defaulting to true.
#[test]
fn the_control_enabled_flag_is_carried() {
    let (path, text) = fixture("control.toml");
    let config = config::parse(&path, &text).unwrap();
    assert!(!config.control.enabled);
    assert!(
        !config.skipped.iter().any(|key| key.starts_with("control")),
        "{:?}",
        config.skipped
    );
    // The default, with no `[control]` table at all, is on.
    let (path, text) = fixture("minimal.toml");
    let config = config::parse(&path, &text).unwrap();
    assert!(config.control.enabled);
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

/// The `providers-model-routing.toml` fixture (ADR 0026 D10): the aliases are canonicalized,
/// `prefer`, `base_urls` and the ordered rules are carried, and a rules-only table parses as an
/// overlay (whether an auth entry exists is decided at resolution, not parse).
#[test]
fn the_routing_fixture_carries_rules_prefer_and_base_urls() {
    let (path, text) = fixture("providers-model-routing.toml");
    let config = config::parse(&path, &text).unwrap();
    let gateway = &config.providers["gemini-gateway"];
    assert_eq!(
        gateway.kind, "openai-compatible",
        "the alias's canonical name"
    );
    assert_eq!(
        gateway.prefer.as_ref().unwrap(),
        &[
            "anthropic".to_owned(),
            "openai-responses".to_owned(),
            "openai-compatible".to_owned()
        ]
    );
    assert_eq!(
        gateway.base_urls["openai-responses"],
        "http://127.0.0.1:3425/v1"
    );
    assert_eq!(
        gateway.models,
        vec![
            radian_host::ModelRuleDecl {
                pattern: "claude/*".to_owned(),
                kind: "anthropic".to_owned()
            },
            radian_host::ModelRuleDecl {
                pattern: "codex/*".to_owned(),
                kind: "openai-responses".to_owned()
            }
        ]
    );
    let overlay = &config.providers["gateway-b"];
    assert!(!overlay.is_full());
    assert!(overlay.kind.is_empty());
    assert_eq!(overlay.models.len(), 1);
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

/// `model-smol.toml` (ADR 0008 §1, ADR 0043 §1): `[model.smol]` carries the cheap route
/// slot, and nothing is reported as skipped.
#[test]
fn the_model_smol_fixture_carries_the_route() {
    let (path, text) = fixture("model-smol.toml");
    let config = config::parse(&path, &text).unwrap();
    let smol = config.model.smol.as_ref().expect("smol is set");
    assert_eq!(smol.provider.as_str(), "openai-compatible");
    assert_eq!(smol.model.as_str(), "m-cheap");
    assert!(config.skipped.is_empty(), "{:?}", config.skipped);
}

/// `mcp-servers.toml` (ADR 0008 §8): the `[[mcp.servers]]` form is carried, two
/// servers, no warnings, and `mcp` is never a skipped key.
#[test]
fn the_mcp_servers_fixture_is_carried() {
    let (path, text) = fixture("mcp-servers.toml");
    let config = config::parse(&path, &text).unwrap();
    let ids: Vec<&str> = config
        .mcp
        .servers
        .iter()
        .map(|server| server.id.as_str())
        .collect();
    assert_eq!(ids, vec!["files", "timer"]);
    assert!(config.mcp.warnings.is_empty(), "{:?}", config.mcp.warnings);
    assert!(
        !config.skipped.iter().any(|key| key == "mcp"),
        "{:?}",
        config.skipped
    );
}

/// `mcp-mcp-servers.toml` (ADR 0008 §8): the shared form is carried with the `HOME`
/// default; the remote entry is skipped with a warning and starts nothing.
#[test]
fn the_mcp_mcp_servers_fixture_skips_the_remote_entry() {
    let (path, text) = fixture("mcp-mcp-servers.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.mcp.servers.len(), 1);
    assert_eq!(config.mcp.servers[0].id, "files");
    assert_eq!(
        config.mcp.servers[0].inherit_env,
        vec![std::ffi::OsString::from("HOME")]
    );
    assert_eq!(config.mcp.warnings.len(), 1, "{:?}", config.mcp.warnings);
    assert!(
        config.mcp.warnings[0].contains("skipped `web`"),
        "{:?}",
        config.mcp.warnings
    );
}

/// `minimal.toml` carries its `[[mcp.servers]]` entry: the user config's example server
/// is wired now, not skipped.
#[test]
fn the_minimal_fixture_mcp_server_is_wired() {
    let (path, text) = fixture("minimal.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.mcp.servers.len(), 1);
    assert_eq!(config.mcp.servers[0].id, "files");
    assert!(
        !config.skipped.iter().any(|key| key == "mcp"),
        "{:?}",
        config.skipped
    );
}
/// `memory-injection.toml` (ADR 0021): the two keys this revision adds
/// are read; the rest of D6's table is known but unwired, so it is reported as skipped rather
/// than refused.
#[test]
fn the_memory_injection_keys_are_read_and_the_rest_is_reported() {
    let (path, text) = fixture("memory-injection.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.memory.additions, Some(false));
    assert_eq!(config.memory.addition_budget, Some(750));
    let injection = config.memory.injection();
    assert!(!injection.additions);
    assert_eq!(injection.addition_budget, 750);
    // Everything the file did not set keeps the documented default.
    assert_eq!(injection.max_tokens, 2000);
    for key in [
        "memory.injection.max_tokens",
        "memory.injection.global_budget",
        "memory.injection.project_budget",
        "memory.injection.overflow",
    ] {
        assert!(
            config.skipped.contains(&key.to_string()),
            "{:?}",
            config.skipped
        );
    }
    assert!(
        !config
            .skipped
            .iter()
            .any(|key| key.starts_with("memory.injection.additions")),
        "{:?}",
        config.skipped
    );
}

/// The minimal fixture carries the known `tui.mouse` preference (`false`): the reader keeps
/// the value, so the TUI can honor it (ADR 0023 U2, plan B).
#[test]
fn the_tui_mouse_preference_is_read_from_the_minimal_fixture() {
    let (path, text) = fixture("minimal.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.tui.mouse, Some(false));
}

/// `tui-bell.toml` (terminal bell): `bell = "attention"` is carried raw; the TUI parses it.
/// An unknown spelling is skipped with a warning (the `tui.*` display-field rule), and the
/// TUI falls back to `off`.
#[test]
fn the_tui_bell_preference_is_carried_and_unknown_falls_back() {
    let (path, text) = fixture("tui-bell.toml");
    let config = config::parse(&path, &text).unwrap();
    assert_eq!(config.tui.bell.as_deref(), Some("attention"));
    assert!(
        !config.skipped.iter().any(|key| key == "tui.bell"),
        "{:?}",
        config.skipped
    );

    let path = PathBuf::from("bell-unknown");
    let config = config::parse(&path, "schema_version = 1\n[tui]\nbell = \"loud\"\n").unwrap();
    assert_eq!(config.tui.bell, None);
    assert!(
        config.skipped.contains(&"tui.bell".to_string()),
        "{:?}",
        config.skipped
    );
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

/// ADR 0027: the `command`/`period` item parameters are carried by the
/// reader, and `command_specs` turns them into specs. v1 has no project-level config layer
/// (ADR 0031), so every configuration source is the user's own and is accepted.
#[test]
fn the_command_fixture_carries_command_and_period() {
    let (path, text) = fixture("tui-footer-command.toml");
    let config = config::parse(&path, &text).unwrap();
    let weather = &config.tui.footer.items["weather"];
    let param = |key: &str| {
        weather
            .params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(param("command"), Some("printf 72F"));
    assert_eq!(param("period"), Some("300"));

    let (specs, warnings) = radian_host::command_specs(&config.tui.footer);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(specs.len(), 2, "{specs:?}");
    let weather = specs.iter().find(|spec| spec.id == "weather").unwrap();
    assert_eq!(weather.program, std::ffi::OsString::from("printf"));
    assert_eq!(weather.args, vec![std::ffi::OsString::from("72F")]);
    assert_eq!(weather.period, Some(std::time::Duration::from_secs(300)));
    let host_tag = specs.iter().find(|spec| spec.id == "host_tag").unwrap();
    assert_eq!(host_tag.period, None, "period = 0 is startup-only");
}
