//! The Rust reader must agree with `docs/schema/check-config-fixtures.sh`'s `EXPECT` table
//! (ADR 0008 §7). The table is duplicated here on purpose: if the two ever disagree, the
//! reader is wrong, and this test fails rather than the checker being bent to match.
//!
//! `readonly_or_refuse` is `readonly` here: this reader interprets a higher-version file (it
//! must never rewrite it, but reading is allowed).

use std::path::PathBuf;

use radian_host::ConfigOrigin;
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
        ("model-retries.toml", "accept"),
        ("skills-extra-paths.toml", "accept"),
        ("skills-builtin.toml", "accept"),
        ("shell-auto-background.toml", "accept"),
        ("memory-injection.toml", "accept"),
        ("integrations-herdr.toml", "accept"),
        ("control.toml", "accept"),
        ("orchestration.toml", "accept"),
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

/// `control.toml` (ADR 0030 O6, slice 8): `[control] enabled` is carried, defaulting to true.
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

/// The `orchestration.toml` fixture (ADR 0030 slice 9): `[orchestration]` carries the user-level
/// orchestration config; the project-level file overrides it key by key.
#[test]
fn the_orchestration_fixture_carries_every_key() {
    let (path, text) = fixture("orchestration.toml");
    let config = config::parse(&path, &text).unwrap();
    let orchestration = &config.orchestration;
    assert_eq!(orchestration.max_tasks, Some(4));
    assert_eq!(orchestration.isolation.as_deref(), Some("worktree"));
    assert_eq!(orchestration.model.as_deref(), Some("opencode-go/glm-5.7"));
    assert_eq!(orchestration.profile.as_deref(), Some("workspace"));
    assert_eq!(
        orchestration.worktree_root.as_deref(),
        Some("/work/example/.radian-worktrees")
    );
    assert_eq!(
        orchestration.verify.as_deref(),
        Some(
            [
                "cargo fmt --all -- --check".to_owned(),
                "cargo test --workspace --locked".to_owned()
            ]
            .as_slice()
        )
    );
    assert_eq!(
        orchestration.policy.as_deref(),
        Some("/work/example/.radian/policy.sh")
    );
    assert_eq!(orchestration.heavy_slots, Some(2));
    assert_eq!(orchestration.stall_after_secs, Some(600));
    assert_eq!(orchestration.merge_needs_human, Some(true));
    assert!(config.skipped.is_empty(), "{:?}", config.skipped);
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

/// ADR 0027: the `command`/`period` item parameters are carried by the
/// reader, and `command_specs` turns them into specs (or refuses them at project level).
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

    let (specs, warnings) = radian_host::command_specs(&config.tui.footer, ConfigOrigin::User);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(specs.len(), 2, "{specs:?}");
    let weather = specs.iter().find(|spec| spec.id == "weather").unwrap();
    assert_eq!(weather.program, std::ffi::OsString::from("printf"));
    assert_eq!(weather.args, vec![std::ffi::OsString::from("72F")]);
    assert_eq!(weather.period, Some(std::time::Duration::from_secs(300)));
    let host_tag = specs.iter().find(|spec| spec.id == "host_tag").unwrap();
    assert_eq!(host_tag.period, None, "period = 0 is startup-only");

    // A project-level config may not enable a command: every entry is refused with a warning.
    let (specs, warnings) = radian_host::command_specs(&config.tui.footer, ConfigOrigin::Project);
    assert!(specs.is_empty(), "{specs:?}");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings.iter().all(|warning| warning.contains("command")),
        "{warnings:?}"
    );
}

/// A scratch directory for the provenance tests (never the real config or home).
fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-config-origin-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// ADR 0027, ADR 0031: provenance is decided by **location**, not by
/// whether the path is the default. A file inside the workspace is project-level; a file outside
/// it — an explicit `--config` path included — is user-level.
#[test]
fn a_config_inside_the_workspace_is_project_level() {
    let workspace = scratch("ws");
    let source = workspace.join("config.toml");
    std::fs::write(&source, "schema_version = 1\n").unwrap();
    assert_eq!(
        config::classify_origin(Some(&source), Some(&workspace), None),
        ConfigOrigin::Project
    );

    // An explicit path outside the workspace is not project-level merely because it is not the
    // default.
    let outside = scratch("outside");
    let elsewhere = outside.join("config.toml");
    std::fs::write(&elsewhere, "schema_version = 1\n").unwrap();
    assert_eq!(
        config::classify_origin(Some(&elsewhere), Some(&workspace), None),
        ConfigOrigin::User
    );

    // No file, and no workspace, are user-level.
    assert_eq!(
        config::classify_origin(None, Some(&workspace), None),
        ConfigOrigin::User
    );
    assert_eq!(
        config::classify_origin(Some(&source), None, None),
        ConfigOrigin::User
    );

    // A file under the user config directory stays user-level even when the workspace is the
    // home: the user's own `~/.radian/config/config.toml` must never be refused.
    let home = scratch("home");
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let user_config = config_dir.join("config.toml");
    std::fs::write(&user_config, "schema_version = 1\n").unwrap();
    assert_eq!(
        config::classify_origin(Some(&user_config), Some(&home), Some(&config_dir)),
        ConfigOrigin::User
    );

    // A workspace that itself lives under `$RADIAN_HOME` (a task worktree under
    // `$RADIAN_HOME/worktrees/`) is **not** swallowed by the config-dir rule: its own config is
    // still project-level.
    let worktrees = config_dir.parent().unwrap().join("worktrees");
    let task_ws = worktrees.join("t1");
    std::fs::create_dir_all(&task_ws).unwrap();
    let task_config = task_ws.join("config.toml");
    std::fs::write(&task_config, "schema_version = 1\n").unwrap();
    assert_eq!(
        config::classify_origin(Some(&task_config), Some(&task_ws), Some(&config_dir)),
        ConfigOrigin::Project
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&outside);
    let _ = std::fs::remove_dir_all(&home);
}

/// `load_in_workspace` reads exactly like `load` and only adds the origin classification; `load`
/// itself stays user-level because it has no workspace.
#[test]
fn load_in_workspace_classifies_the_origin() {
    let workspace = scratch("load");
    let source = workspace.join("config.toml");
    std::fs::write(&source, "schema_version = 1\n").unwrap();
    let loaded = radian_host::load_config_in_workspace(Some(&source), Some(&workspace)).unwrap();
    assert_eq!(loaded.origin, ConfigOrigin::Project);
    assert_eq!(loaded.source.as_deref(), Some(source.as_path()));

    let outside = scratch("load-out");
    let elsewhere = outside.join("config.toml");
    std::fs::write(&elsewhere, "schema_version = 1\n").unwrap();
    let loaded = radian_host::load_config_in_workspace(Some(&elsewhere), Some(&workspace)).unwrap();
    assert_eq!(loaded.origin, ConfigOrigin::User);

    let loaded = radian_host::load_config(Some(&source)).unwrap();
    assert_eq!(
        loaded.origin,
        ConfigOrigin::User,
        "the old entry point never claims a project origin"
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&outside);
}
