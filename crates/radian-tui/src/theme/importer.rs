//! The one-time pi theme import (ADR 0029).
//!
//! `radian theme import <pi-theme.json>` converts a pi theme JSON to Radian's TOML theme. The
//! pi → Radian role mapping lives **only** here and in [`super::file`]'s `fg_source`/
//! `bg_source` (which the legacy `.json` loader and `[tui.colors]`'s pi-name transition also
//! use); at run time the TOML loader knows Radian role names only. The conversion is code, not
//! judgement: the mapping table is fixed, so the same input always gives byte-identical TOML.
//!
//! The conversion resolves pi's `vars` and `appearance` (through [`resolve_pi_colors`], the
//! same routine the runtime loader uses) and writes each Radian role as a literal color, so the
//! result needs no variables. It reports every pi key it drops and every role that keeps the
//! built-in value.

use serde_json::Value;

use super::Role;
use super::color::ColorValue;
use super::file::{PI_ROLES, bg_source, fg_source, resolve_pi_colors, targets_for_pi_name};

/// The outcome of converting one pi theme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// The theme name: `--name`, else the file's `name` field, else the fallback (file stem).
    pub name: String,
    /// The TOML theme text, deterministic.
    pub toml: String,
    /// The report lines — dropped pi keys, roles that fell back, variable problems.
    pub report: Vec<String>,
}

/// Converts a pi theme JSON `text` to a Radian TOML theme. `name` is `--name` (it wins over the
/// file's own `name`); `fallback_name` is used when neither exists (the caller passes the file
/// stem). `Err` is a one-line reason (not valid JSON, no usable name, ...).
pub fn convert(text: &str, name: Option<&str>, fallback_name: &str) -> Result<Converted, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("not valid JSON ({error})"))?;
    let table = value
        .as_object()
        .ok_or_else(|| "the top level is not an object".to_owned())?;

    let theme_name = name
        .map(str::to_owned)
        .or_else(|| table.get("name").and_then(Value::as_str).map(str::to_owned))
        .filter(|candidate| !candidate.trim().is_empty())
        .unwrap_or_else(|| fallback_name.to_owned());
    if !valid_name(&theme_name) {
        return Err(format!(
            "theme name {theme_name:?} is not usable (letters, digits, `-`, `_`, `.`)"
        ));
    }

    let mut report = Vec::new();
    let appearance = match table.get("appearance") {
        None | Some(Value::Null) => "dark",
        Some(Value::String(text)) if text == "dark" || text == "light" => text.as_str(),
        Some(other) => {
            report.push(format!(
                "appearance {other} is not \"dark\" or \"light\"; assuming dark"
            ));
            "dark"
        }
    };

    let empty = serde_json::Map::new();
    let vars = table
        .get("vars")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let Some(colors) = table.get("colors").and_then(Value::as_object) else {
        report.push("no `colors` table: this file carries no roles".to_owned());
        return Ok(Converted {
            name: theme_name.clone(),
            toml: render(&theme_name, appearance, &[]),
            report,
        });
    };
    let (resolved, problems) = resolve_pi_colors(colors, vars);
    report.extend(problems.into_variable_warnings(&theme_name));

    // The roles a pi file feeds, in `Role::ALL` order; the rest fall back to the built-in.
    let mut entries: Vec<(Role, Option<ColorValue>, Option<ColorValue>)> = Vec::new();
    let mut fell_back: Vec<&str> = Vec::new();
    for role in Role::ALL {
        let foreground = fg_source(role).and_then(|key| resolved.get(key).copied());
        let background = bg_source(role).and_then(|key| resolved.get(key).copied());
        if foreground.is_none() && background.is_none() {
            fell_back.push(role.as_str());
        } else {
            entries.push((role, foreground, background));
        }
    }
    if !fell_back.is_empty() {
        report.push(format!(
            "role(s) with no value in the file, using the built-in {appearance}: {}",
            fell_back.join(", ")
        ));
    }
    // Every pi key the file carries that no Radian role consumes, plus the top-level `export`
    // (ADR 0029). Sorted for determinism.
    let mut dropped: Vec<String> = colors
        .keys()
        .filter(|key| targets_for_pi_name(key).is_empty())
        .map(|key| {
            if PI_ROLES.contains(&key.as_str()) {
                key.clone()
            } else {
                format!("{key} (unknown)")
            }
        })
        .collect();
    if table.contains_key("export") {
        dropped.push("export".to_owned());
    }
    dropped.sort();
    if !dropped.is_empty() {
        report.push(format!("dropped pi key(s): {}", dropped.join(", ")));
    }

    Ok(Converted {
        name: theme_name.clone(),
        toml: render(&theme_name, appearance, &entries),
        report,
    })
}

/// Writes a converted theme to `<dir>/<name>.toml` (ADR 0029). Without
/// `force` an existing file is refused (never overwritten). The directory is created when
/// missing. Returns the path written.
pub fn write(
    dir: &std::path::Path,
    name: &str,
    toml: &str,
    force: bool,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let path = dir.join(format!("{name}.toml"));
    let mut bytes = toml.to_owned().into_bytes();
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    if force {
        std::fs::write(&path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    } else {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "{} already exists; use --force to overwrite it",
                        path.display()
                    )
                } else {
                    format!("cannot write {}: {error}", path.display())
                }
            })?;
        file.write_all(&bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(path)
}

/// A theme name usable as an `<name>.toml` file name (the same rule the loader enforces).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "system"
        && name != ".."
        && name.chars().count() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

/// Renders the TOML text: `name`, `appearance`, then the `[roles]` table in `Role::ALL` order.
fn render(
    name: &str,
    appearance: &str,
    entries: &[(Role, Option<ColorValue>, Option<ColorValue>)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("name = \"{name}\"\n"));
    out.push_str(&format!("appearance = \"{appearance}\"\n"));
    if entries.is_empty() {
        return out;
    }
    out.push_str("\n[roles]\n");
    for (role, foreground, background) in entries {
        out.push_str(&format!(
            "{} = {}\n",
            role.as_str(),
            role_value(*foreground, *background)
        ));
    }
    out
}

/// One role's TOML value: a bare color for a foreground only, a table when a background is set.
fn role_value(foreground: Option<ColorValue>, background: Option<ColorValue>) -> String {
    match (foreground, background) {
        (Some(foreground), None) => color(foreground),
        (None, Some(background)) => format!("{{ bg = {} }}", color(background)),
        (Some(foreground), Some(background)) => {
            format!(
                "{{ fg = {}, bg = {} }}",
                color(foreground),
                color(background)
            )
        }
        (None, None) => unreachable!("a role entry carries a foreground or a background"),
    }
}

/// A resolved color as TOML: `#rrggbb`, a 0–255 index, or `""` for the terminal default.
fn color(value: ColorValue) -> String {
    match value {
        ColorValue::Rgb(rgb) => format!("\"#{:02x}{:02x}{:02x}\"", rgb.r, rgb.g, rgb.b),
        ColorValue::Indexed(index) => index.to_string(),
        ColorValue::Default => "\"\"".to_owned(),
    }
}

/// The built-in a role keeps when the file has no value for it — used only by the report, so the
/// caller can name the fallback palette. Kept here so the importer and the loader agree.
#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/themes")
    }

    fn fixture_text(stem: &str) -> String {
        let path = fixtures().join(format!("{stem}.json"));
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// The conversion is deterministic and every fixture's golden TOML is exactly what it
    /// produces (ADR 0029 acceptance). Changing a mapping changes the golden.
    #[test]
    fn every_fixture_converts_to_its_committed_golden() {
        let dir = fixtures();
        let mut json: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension()? == "json")
                    .then(|| path.file_stem().unwrap().to_string_lossy().into_owned())
            })
            .collect();
        json.sort();
        assert_eq!(json.len(), 17, "the pi fixture set");
        for stem in json {
            let converted = convert(&fixture_text(&stem), None, &stem).unwrap();
            assert_eq!(converted.name, stem, "the file's own name is used");
            let golden_path = dir.join(format!("{stem}.toml"));
            let golden = std::fs::read_to_string(&golden_path)
                .unwrap_or_else(|error| panic!("{}: {error}", golden_path.display()));
            assert_eq!(
                converted.toml, golden,
                "{stem}: the golden TOML is stale (re-run the importer)"
            );
            // Converting the golden's own source twice is byte-identical (determinism).
            let again = convert(&fixture_text(&stem), None, &stem).unwrap();
            assert_eq!(converted, again, "{stem}: not deterministic");
        }
    }

    /// The report names every dropped pi key and every role that fell back.
    #[test]
    fn the_report_lists_dropped_keys_and_fallback_roles() {
        let text = r##"{
            "name": "t",
            "colors": {
                "text": "#c0caf5",
                "customMessageBg": "#111111",
                "borderMuted": "#222222",
                "bashMode": "#333333"
            },
            "export": "#000000"
        }"##;
        let converted = convert(text, None, "fallback").unwrap();
        let report = converted.report.join("\n");
        assert!(report.contains("dropped pi key(s)"), "{report}");
        assert!(report.contains("customMessageBg"), "{report}");
        assert!(report.contains("borderMuted"), "{report}");
        assert!(report.contains("bashMode"), "{report}");
        assert!(report.contains("export"), "{report}");
        assert!(report.contains("using the built-in dark:"), "{report}");
        // A role the file does feed is not in the fallback list.
        assert!(!converted.toml.contains("selection ="), "no selectedBg");
        assert!(
            converted.toml.contains("text = \"#c0caf5\""),
            "{}",
            converted.toml
        );
    }

    /// `--name` wins over the file's `name`; an unusable name is refused.
    #[test]
    fn the_name_override_wins_and_a_bad_name_is_refused() {
        let text = r##"{"name": "from-file", "colors": {"text": "#010203"}}"##;
        let converted = convert(text, Some("chosen"), "stem").unwrap();
        assert_eq!(converted.name, "chosen");
        assert!(
            converted.toml.starts_with("name = \"chosen\""),
            "{}",
            converted.toml
        );
        assert!(convert(text, Some("a/b"), "stem").is_err());
        assert!(convert(text, Some("system"), "stem").is_err());
    }

    /// The file name fallback is used when neither `--name` nor the file's `name` exists.
    #[test]
    fn the_stem_is_the_last_resort_name() {
        let converted = convert(r##"{"colors": {"text": "#010203"}}"##, None, "my-theme").unwrap();
        assert_eq!(converted.name, "my-theme");
    }

    /// ADR 0029: the write refuses an existing `<name>.toml` without
    /// `--force`, and `--force` overwrites it.
    #[test]
    fn write_refuses_an_existing_file_without_force() {
        let dir = std::env::temp_dir().join(format!("radian-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = write(&dir, "mine", "name = \"mine\"\n", false).unwrap();
        assert!(path.ends_with("mine.toml"), "{}", path.display());
        let error = write(&dir, "mine", "name = \"mine\"\n", false).unwrap_err();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name = \"mine\"\n",
            "a refused write leaves the file untouched"
        );
        let again = write(
            &dir,
            "mine",
            "name = \"mine\"\nappearance = \"light\"\n",
            true,
        )
        .unwrap();
        assert_eq!(again, path);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("appearance = \"light\"")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
