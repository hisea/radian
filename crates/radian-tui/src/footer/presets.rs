//! The built-in footer presets (moved from `footer.rs` by pure move, ADR 0043 decision 5):
//! `deck`, `full`, `pi` and `classic` are TOML data files, parsed by the user's config parser.

use radian_host::config::TuiFooterConfig;

use crate::theme::Role;

use super::{ItemStyle, Row, build_rows};

/// Which footer the session shows.
///
/// The `tui.footer.preset` values, in documentation order. The parser and the manual's
/// drift test both read this list.
pub const PRESET_NAMES: &[&str] = &["deck", "full", "pi", "classic"];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// The default (ADR 0027): three rows — the usage widget (absent without data),
    /// the identity row, and the numbers row.
    #[default]
    Deck,
    /// The revision-1 two-row preset: the identity row and the numbers row (`full` also shows `jobs`, this session's running background jobs).
    Full,
    /// The earlier two-row preset: model/cwd/branch/context bar/reasoning, then tokens/cost.
    Pi,
    /// Today's one status line, byte-for-byte (drawn by the renderer).
    Classic,
}

impl Preset {
    /// Every preset name, in parse order. The single source for `parse` and the manual's
    /// drift test: adding a preset means adding it here first (`as_str` is exhaustive, so
    /// the compiler forces a new variant into it, and `parse` is a lookup over this list).
    pub const ALL: &[Self] = &[Self::Deck, Self::Full, Self::Pi, Self::Classic];

    /// The config-file spelling of this preset.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deck => PRESET_NAMES[0],
            Self::Full => PRESET_NAMES[1],
            Self::Pi => PRESET_NAMES[2],
            Self::Classic => PRESET_NAMES[3],
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.as_str() == name)
    }
}

/// One built-in preset's TOML (ADR 0027): `deck`, `full`, `pi`
/// and `classic` are data files under `crates/radian-tui/presets/`, shipped in the binary.
fn preset_source(preset: Preset) -> &'static str {
    match preset {
        Preset::Deck => include_str!("../../presets/deck.toml"),
        Preset::Full => include_str!("../../presets/full.toml"),
        Preset::Pi => include_str!("../../presets/pi.toml"),
        Preset::Classic => include_str!("../../presets/classic.toml"),
    }
}

/// The shipped TOML of a named preset, for `radian footer export <preset>` (ADR 0027
/// ): the exact file bytes, so a user can paste them under
/// `[tui.footer]` and get the same footer. `Err` names the four presets.
pub fn export_preset(name: &str) -> Result<&'static str, String> {
    match Preset::parse(name) {
        Some(preset) => Ok(preset_source(preset)),
        None => Err(format!(
            "unknown footer preset {name:?}; expected one of {}",
            PRESET_NAMES.join(", ")
        )),
    }
}

/// Parses a built-in preset with the user's config parser (ADR 0027):
/// the file holds the config's `[tui.footer]` section verbatim, so it is parsed by
/// `radian_host::config::parse` exactly like a user config. A preset that fails to parse (or
/// skips a key) is a programming error: the all-four-parse test fails on it, so this never
/// runs with a bad file outside a broken build.
pub(crate) fn preset_config(preset: Preset) -> TuiFooterConfig {
    let text = format!("schema_version = 1\n{}", preset_source(preset));
    let config =
        radian_host::config::parse(std::path::Path::new("<built-in footer preset>"), &text)
            .expect("a built-in footer preset parses");
    assert!(
        config.skipped.is_empty(),
        "a built-in footer preset has no skipped keys: {:?}",
        config.skipped
    );
    config.tui.footer
}

/// The built-in preset's rows, resolved like user-configured rows. A `Footer` with no rows
/// (only `Footer::default()`, before any config is applied) draws these through `lines`;
/// `from_config` builds them the same way and then merges the user's per-field `items` config
/// on top, exactly as the old Rust tables did.
pub(crate) fn preset_footer_rows(preset: Preset, warnings: &mut Vec<String>) -> Vec<Row> {
    let preset_cfg = preset_config(preset);
    let separator = preset_cfg
        .separator
        .clone()
        .unwrap_or_else(|| super::DEFAULT_SEPARATOR.to_string());
    build_rows(
        &preset_cfg.rows,
        &separator,
        &ItemStyle::Role(Role::FooterSeparator),
        &std::collections::BTreeMap::new(),
        &std::collections::BTreeSet::new(),
        &mut std::collections::BTreeSet::new(),
        warnings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `Preset::ALL` entry round-trips through `parse(as_str())`: `ALL` is the single
    /// source, so a new preset without a spelling fails here.
    #[test]
    fn every_preset_round_trips_through_parse() {
        for preset in Preset::ALL {
            assert_eq!(Preset::parse(preset.as_str()), Some(*preset), "{preset:?}");
        }
        let spelled: Vec<&str> = Preset::ALL.iter().map(|preset| preset.as_str()).collect();
        assert_eq!(
            spelled, PRESET_NAMES,
            "ALL and PRESET_NAMES list the same presets in order"
        );
    }
}
