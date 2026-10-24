//! The seven plan-mark theme roles (ADR 0048 decision 11): a theme file sets one by name,
//! `[tui.colors]` does the same, a theme without the keys keeps the defaults of its
//! `appearance`, and the pi import maps the five roles pi has a token for.

use std::fs;
use std::path::{Path, PathBuf};

use radian_host::config::TuiColorOverride;
use radian_tui::theme::{Theme, load};
use ratatui::style::Color;

/// A unique scratch directory per test, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-plan-roles-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn overrides(text: &str) -> Vec<(String, TuiColorOverride)> {
    radian_host::config::parse(Path::new("/tmp/c.toml"), text)
        .unwrap()
        .tui
        .colors
}

/// A theme file that sets `plan_running` changes that role only; every other plan role keeps
/// the built-in `dark` default (the file's `appearance`).
#[test]
fn a_theme_file_sets_only_the_plan_role_it_names() {
    let dir = TempDir::new("file");
    fs::write(
        dir.path().join("mine.toml"),
        "name = \"mine\"\n[roles]\nplan_running = \"#123456\"\n",
    )
    .unwrap();
    let (theme, warnings) = load(Some("mine"), false, true, Some(dir.path()), &[]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(theme.plan_running.fg, Some(Color::Rgb(0x12, 0x34, 0x56)));
    let dark = Theme::dark();
    for (name, got, want) in [
        ("plan_needs_you", theme.plan_needs_you, dark.plan_needs_you),
        ("plan_queued", theme.plan_queued, dark.plan_queued),
        ("plan_paused", theme.plan_paused, dark.plan_paused),
        ("plan_done", theme.plan_done, dark.plan_done),
        ("plan_draft", theme.plan_draft, dark.plan_draft),
        ("plan_cancelled", theme.plan_cancelled, dark.plan_cancelled),
    ] {
        assert_eq!(got, want, "{name} keeps the built-in dark default");
    }
    // No other role moved either.
    assert_eq!(theme.hint, dark.hint);
    assert_eq!(theme.status_ok, dark.status_ok);
}

/// `[tui.colors]` accepts the `plan_*` names like any other role.
#[test]
fn tui_colors_sets_a_plan_role() {
    let (theme, warnings) = load(
        None,
        false,
        true,
        None,
        &overrides("schema_version = 1\n[tui.colors]\nplan_running = \"#0a0b0c\"\n"),
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(theme.plan_running.fg, Some(Color::Rgb(0x0a, 0x0b, 0x0c)));
    assert_eq!(
        theme.plan_queued,
        Theme::dark().plan_queued,
        "only the named role changes"
    );
}

/// A theme without the plan keys keeps the defaults of its `appearance` (ADR 0029's existing
/// fallback rule; the plan roles add no fallback of their own).
#[test]
fn a_theme_without_the_plan_keys_keeps_its_appearances_defaults() {
    let dir = TempDir::new("appearance");
    fs::write(
        dir.path().join("paper.toml"),
        "name = \"paper\"\nappearance = \"light\"\n[roles]\ntext = \"#000000\"\n",
    )
    .unwrap();
    let (theme, warnings) = load(Some("paper"), false, true, Some(dir.path()), &[]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let light = Theme::light();
    for (name, got, want) in [
        ("plan_needs_you", theme.plan_needs_you, light.plan_needs_you),
        ("plan_running", theme.plan_running, light.plan_running),
        ("plan_paused", theme.plan_paused, light.plan_paused),
        ("plan_done", theme.plan_done, light.plan_done),
        ("plan_draft", theme.plan_draft, light.plan_draft),
        ("plan_cancelled", theme.plan_cancelled, light.plan_cancelled),
        ("plan_queued", theme.plan_queued, light.plan_queued),
    ] {
        assert_eq!(got, want, "{name} falls back to the light palette");
    }
}

/// `radian theme import` maps the five roles pi has a token for and reports running and paused
/// as unmapped, like every other role with no pi source.
#[test]
fn the_import_maps_five_plan_roles_and_reports_the_other_two() {
    let text = r##"{
        "name": "t",
        "appearance": "dark",
        "colors": {
            "success": "#010203",
            "error": "#040506",
            "warning": "#070809",
            "accent": "#0a0b0c",
            "dim": "#0d0e0f"
        }
    }"##;
    let converted = radian_tui::theme::importer::convert(text, None, "t").unwrap();
    for (key, color) in [
        ("plan_done", "#010203"),
        ("plan_cancelled", "#040506"),
        ("plan_needs_you", "#070809"),
        ("plan_queued", "#0a0b0c"),
        ("plan_draft", "#0d0e0f"),
    ] {
        assert!(
            converted.toml.contains(&format!("{key} = \"{color}\"")),
            "`{key}` takes `{color}`:\n{}",
            converted.toml
        );
    }
    assert!(
        !converted.toml.contains("plan_running"),
        "running has no pi token:\n{}",
        converted.toml
    );
    assert!(
        !converted.toml.contains("plan_paused"),
        "paused has no pi token:\n{}",
        converted.toml
    );
    let report = converted.report.join("\n");
    assert!(
        report.contains("using the built-in dark:"),
        "the fallback line is reported:\n{report}"
    );
    assert!(report.contains("plan_running"), "{report}");
    assert!(report.contains("plan_paused"), "{report}");
}
