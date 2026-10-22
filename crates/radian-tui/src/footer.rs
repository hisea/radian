//! The footer's rows and built-in items (ADR 0027 C2, slice 1).
//!
//! The footer is rows of `left`/`right` item groups. Each item renders text from the session's
//! facts (the projection, `App`'s startup facts) with its own placeholders, glyph and theme
//! role. **The draw path does no I/O**: `cwd` and `git_branch` are startup facts refreshed by
//! the host ([`crate::app::App::set_git_branch`]), and nothing here runs git. Colors come only
//! from theme roles (ADR 0029). An unknown item id, placeholder or parameter is shown raw
//! with one warning and never blocks startup (the ADR 0008 `tui.*` rule).

use std::borrow::Cow;

use radian_host::config::{TuiFooterConfig, TuiFooterItemConfig};
use ratatui::text::Line;

use crate::app::App;
use crate::theme::Role;

mod components;
mod draw;
mod presets;
mod rows;

pub(crate) use components::{age_text, tick_in};
use draw::render_row;
pub(crate) use draw::{
    auto_marker, context_fraction, contrib_role, contribution, contribution_text, inline_styles,
    role_style,
};
pub(crate) use presets::preset_footer_rows;
pub use presets::{Preset, export_preset};
pub(crate) use rows::{
    Item, ItemStyle, Row, build_rows, canonical_id, is_command_item, merge_into_item,
    parse_item_style,
};
pub use rows::{RowPosition, fold_legacy_widgets};

/// Every built-in footer item id, in registry order. The single source for the manual's
/// drift test: a new built-in means a new entry in the component registry first.
pub fn footer_item_ids() -> Vec<&'static str> {
    components::ids_all()
}

/// The `{placeholder}`s one built-in id offers, or `None` when no component claims the id.
pub fn footer_item_placeholders(id: &str) -> Option<&'static [&'static str]> {
    components::placeholders(id)
}

/// The parameters one built-in id reads from its item (an empty list when it reads none),
/// or `None` when no component claims the id. Each component declares its own list by
/// overriding `Component::params`.
pub fn footer_item_params(id: &str) -> Option<&'static [&'static str]> {
    components::params_of(id)
}

/// The brand mark for a glyph set (ADR 0027): the Nerd Font `md-alpha_r_circle_
/// outline`, the plain bold-accent `R` (the same font as the body, so it sits on the baseline),
/// or the ASCII word `rad`. `㎭` (U+33AD) is East-Asian-Wide and its fallback glyph sits off the
/// baseline, so it is no longer used.
pub fn brand_mark(glyphs: Glyphs) -> &'static str {
    match glyphs {
        Glyphs::Nerd => "\u{f0c20}",
        Glyphs::Unicode => "R",
        Glyphs::Ascii => "rad",
        // The emoji set's mark (ADR 0027): a high-voltage bolt, one
        // cell, drawn by the system emoji font like every other emoji-tier glyph.
        Glyphs::Emoji => "⚡",
    }
}

/// How the built-in items draw their icons (ADR 0027 C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Glyphs {
    /// Unicode box/geometric glyphs (the default).
    #[default]
    Unicode,
    /// Nerd Font code points (the pi footer).
    Nerd,
    /// Pure ASCII.
    Ascii,
    /// Emoji (ADR 0027): drawn by the system emoji font, so the
    /// six-font coverage rule does not apply to this tier.
    Emoji,
}

impl Glyphs {
    /// Every glyph set, in parse order. The single source for `parse` and the manual's drift
    /// test: adding a set means adding it here first (`as_str` is exhaustive, so the
    /// compiler forces a new variant into it, and `parse` is a lookup over this list).
    pub const ALL: &[Self] = &[Self::Unicode, Self::Nerd, Self::Ascii, Self::Emoji];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.as_str() == name)
    }

    /// The config-file spelling of this set.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unicode => "unicode",
            Self::Nerd => "nerd",
            Self::Ascii => "ascii",
            Self::Emoji => "emoji",
        }
    }

    /// The card frame's characters (ADR 0028 rule 2): unicode box drawing for the
    /// unicode, nerd and emoji sets, `+ - |` for ascii.
    pub fn frame(self) -> FrameGlyphs {
        match self {
            Self::Unicode | Self::Nerd | Self::Emoji => FrameGlyphs {
                top_left: "┌",
                top_right: "┐",
                bottom_left: "└",
                bottom_right: "┘",
                tee_left: "├",
                tee_right: "┤",
                horizontal: "─",
                vertical: "│",
                bracket_left: "⟦",
                bracket_right: "⟧",
            },
            Self::Ascii => FrameGlyphs {
                top_left: "+",
                top_right: "+",
                bottom_left: "+",
                bottom_right: "+",
                tee_left: "+",
                tee_right: "+",
                horizontal: "-",
                vertical: "|",
                bracket_left: "[",
                bracket_right: "]",
            },
        }
    }

    /// The tool-call status glyphs (ADR 0028 rule 4): running, success, failure.
    /// The unicode set is covered by all six checked fonts (ADR 0027): omp's
    /// `⟳`/`✔`/`✘` are not, so `~`/`✓`/`×` stand in. The Nerd set uses the Font Awesome
    /// codepoints; the ASCII set is plain. The emoji set draws the hourglass and the ballot
    /// boxes from the system emoji font (ADR 0027).
    pub fn tool_status(self) -> [&'static str; 3] {
        match self {
            Self::Unicode => ["~", "✓", "×"],
            Self::Nerd => ["\u{f110}", "\u{f00c}", "\u{f00d}"],
            Self::Ascii => ["*", "+", "x"],
            Self::Emoji => ["⏳", "✅", "❌"],
        }
    }

    /// The marker column's **folded** spelling (ADR 0035's 2026-10-16 "折叠标记统一" note): the
    /// thinking channel's folded one-liner and an expandable tool head that shows only its head
    /// both take it. `◼ ` on the unicode, nerd and emoji tiers, `+ ` under `ascii`. `◼`/`◻`
    /// miss four of the six reference fonts: the pair was chosen anyway, a documented
    /// exception like the ask record's `◉`/`○`.
    pub fn fold_marker(self) -> &'static str {
        match self {
            Self::Unicode | Self::Nerd | Self::Emoji => "◼ ",
            Self::Ascii => "+ ",
        }
    }

    /// The marker column's **expanded** spelling (same note): a body drawn under the head (the
    /// `errors` preview, an error body, `Ctrl-O`/search expansion, the expanded thinking
    /// channel). `◻ ` on the unicode, nerd and emoji tiers, `- ` under `ascii`.
    pub fn open_marker(self) -> &'static str {
        match self {
            Self::Unicode | Self::Nerd | Self::Emoji => "◻ ",
            Self::Ascii => "- ",
        }
    }
}

/// How the gap between a row's left and right groups is drawn (ADR 0027 item 4,
/// following omp's `statusLine.contextLine`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextLine {
    /// The gap is spaces (the default; Radian's footer has no background fill, so a line by
    /// default would be noise - omp's `off` still draws a solid accent line).
    #[default]
    Off,
    /// The gap is a `─` rule: the used fraction of the context window in the `budget` role,
    /// the rest in the `footer_separator` role (omp's `percentage`). With no known budget the
    /// gap stays spaces: never a fabricated percentage.
    Percentage,
}

impl ContextLine {
    /// Every gap-fill name. The single source for `parse` and the manual's drift test.
    pub const ALL: &[Self] = &[Self::Off, Self::Percentage];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|v| v.as_str() == name)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Percentage => "percentage",
        }
    }
}

/// The characters a card frame draws (ADR 0028).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameGlyphs {
    pub top_left: &'static str,
    pub top_right: &'static str,
    pub bottom_left: &'static str,
    pub bottom_right: &'static str,
    pub tee_left: &'static str,
    pub tee_right: &'static str,
    pub horizontal: &'static str,
    pub vertical: &'static str,
    /// The meta row's brackets (ADR 0028 rule 3).
    pub bracket_left: &'static str,
    pub bracket_right: &'static str,
}

/// The separator between two items when neither the footer nor the row sets one.
pub const DEFAULT_SEPARATOR: &str = " · ";
/// The separator's style when none is set: the `footer_separator` theme role.
pub const DEFAULT_SEPARATOR_STYLE: Role = Role::FooterSeparator;
/// The session's footer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Footer {
    preset: Preset,
    glyphs: Glyphs,
    rows: Vec<Row>,
    /// The left/right gap fill (ADR 0027 item 4).
    context_line: ContextLine,
    /// The footer inherits the terminal's background (ADR 0027 item 5): every
    /// cell's background is cleared, a configured item style's included.
    transparent: bool,
}

impl Footer {
    /// Parses the configuration. Unknown item ids, placeholders and parameters are shown raw;
    /// their warnings are collected here (once) and shown at startup.
    pub fn from_config(config: &TuiFooterConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        // The old item ids already warned about (ADR 0027: an earlier
        // version's id keeps working for one version, with one startup warning naming the new
        // one).
        let mut aliased: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let preset = match config.preset.as_deref() {
            None => Preset::Deck,
            Some(name) => Preset::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.footer.preset = {name:?} is not one of deck|full|pi|classic; using deck"
                ));
                Preset::Deck
            }),
        };
        let glyphs = match config.glyphs.as_deref() {
            None => Glyphs::Unicode,
            Some(name) => Glyphs::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.footer.glyphs = {name:?} is not one of unicode|nerd|ascii|emoji; using unicode"
                ));
                Glyphs::Unicode
            }),
        };
        let separator = config
            .separator
            .clone()
            .unwrap_or_else(|| DEFAULT_SEPARATOR.to_string());
        let separator_style = match config.separator_style.as_deref() {
            None => ItemStyle::Role(DEFAULT_SEPARATOR_STYLE),
            Some(name) => parse_item_style(name).unwrap_or_else(|error| {
                warnings.push(format!("tui.footer: bad separator_style {name:?}: {error}"));
                ItemStyle::Role(Role::FooterSeparator)
            }),
        };
        let context_line = match config.context_line.as_deref() {
            None => ContextLine::Off,
            Some(name) => ContextLine::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.footer.context_line = {name:?} is not one of off|percentage; using off"
                ));
                ContextLine::Off
            }),
        };
        let transparent = config.transparent.unwrap_or(false);
        // The ids configured as out-of-process `command` items (ADR 0027).
        // Such an id is not an unknown-id typo: until the background producer publishes a value
        // the item hides (never a raw id), and `{value}` is its one placeholder. Only an id no
        // built-in claims can be a command item — a built-in always wins its id.
        let command_ids: std::collections::BTreeSet<String> = config
            .items
            .iter()
            .filter(|(id, item)| components::builtin(id).is_none() && is_command_item(item))
            .map(|(id, _)| id.clone())
            .chain(
                config
                    .rows
                    .iter()
                    .flat_map(|row| row.left.iter().chain(row.right.iter()))
                    .filter(|item| components::builtin(&item.id).is_none() && is_command_item(item))
                    .map(|item| item.id.clone()),
            )
            .collect();
        // The per-field `items` table, with an earlier version's usage ids canonicalized
        // (ADR 0027), so an old id written in `[tui.footer.items]` lands
        // on the `usage` item. A `usage` block wins when both are written.
        let mut items: std::collections::BTreeMap<String, TuiFooterItemConfig> = Default::default();
        for (id, field) in &config.items {
            let id = canonical_id(id, &mut aliased, &mut warnings);
            items.insert(id, field.clone());
        }
        let rows: Vec<Row> = if config.rows.is_empty() {
            // The built-in preset, then the per-field `items` config on top.
            let mut rows = preset_footer_rows(preset, &mut warnings);
            for row in &mut rows {
                for item in row.left.iter_mut().chain(row.right.iter_mut()) {
                    if let Some(field) = items.get(&item.id) {
                        merge_into_item(item, field, &mut warnings);
                    }
                }
            }
            for row in &mut rows {
                row.left.retain(|item| !item.disabled);
                row.right.retain(|item| !item.disabled);
            }
            rows
        } else {
            build_rows(
                &config.rows,
                &separator,
                &separator_style,
                &items,
                &command_ids,
                &mut aliased,
                &mut warnings,
            )
        };
        (
            Self {
                preset,
                glyphs,
                rows,
                context_line,
                transparent,
            },
            warnings,
        )
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    /// The gap fill between a row's left and right groups.
    pub fn context_line(&self) -> ContextLine {
        self.context_line
    }

    /// Whether every footer cell's background is cleared.
    pub fn transparent(&self) -> bool {
        self.transparent
    }

    /// The configured glyph set (the renderer reads it for other chrome too).
    pub fn glyphs(&self) -> Glyphs {
        self.glyphs
    }

    /// Whether the `jobs` item is part of the footer: the idle loop only ticks the age while
    /// it shows (ADR 0023 U2). An empty row list is the pre-config default, which draws the
    /// preset's rows (`lines` falls back the same way).
    pub(crate) fn shows_jobs(&self) -> bool {
        fn has_jobs(rows: &[Row]) -> bool {
            rows.iter()
                .flat_map(|row| row.left.iter().chain(row.right.iter()))
                .any(|item: &Item| item.id == "jobs" && !item.disabled)
        }
        if self.rows.is_empty() {
            let mut warnings = Vec::new();
            return has_jobs(&preset_footer_rows(self.preset, &mut warnings));
        }
        has_jobs(&self.rows)
    }

    /// The footer's lines at `width`. The `classic` preset is the renderer's (it draws the
    /// pre-ADR one-line status bar, so a `classic` footer has no rows). Only rows positioned at
    /// the footer (ADR 0027); the input-adjacent rows draw through
    /// [`Footer::above_input_lines`] and [`Footer::below_input_lines`].
    pub fn lines(&self, app: &App, width: usize) -> Vec<Line<'static>> {
        let mut empty_warnings = Vec::new();
        let preset;
        let rows: Cow<'_, [Row]> = if self.rows.is_empty() {
            preset = preset_footer_rows(self.preset, &mut empty_warnings);
            Cow::Owned(preset)
        } else {
            Cow::Borrowed(&self.rows)
        };
        // A row with no data renders empty (the deck's usage row, revision 3): drop it so it
        // never leaves a blank line.
        rows.iter()
            .filter(|row| row.position == RowPosition::Footer)
            .map(|row| render_row(app, row, width, self.context_line))
            .filter(|line| !line.spans.is_empty())
            .map(|mut line| {
                if self.transparent {
                    // ADR 0027 item 5: no background, so a transparent terminal
                    // shows through. The gap's spaces and the gauge keep their own colors.
                    for span in &mut line.spans {
                        span.style.bg = None;
                    }
                }
                line
            })
            .collect()
    }

    /// The rows positioned above the input box (ADR 0027), at `width`.
    /// Empty rows are dropped, so a row with no data takes no space.
    pub fn above_input_lines(&self, app: &App, width: usize) -> Vec<Line<'static>> {
        self.position_lines(app, width, RowPosition::AboveInput)
    }

    /// The rows positioned below the input box (ADR 0027), at `width`.
    pub fn below_input_lines(&self, app: &App, width: usize) -> Vec<Line<'static>> {
        self.position_lines(app, width, RowPosition::BelowInput)
    }

    fn position_lines(&self, app: &App, width: usize, position: RowPosition) -> Vec<Line<'static>> {
        self.rows
            .iter()
            .filter(|row| row.position == position)
            .map(|row| render_row(app, row, width, self.context_line))
            .filter(|line| !line.spans.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-ASCII glyph the `unicode` tier can draw (ADR 0027, plus
    /// ADR 0035's band half cells and the glyph note; `×` for a
    /// failed tool head, `•` for a one-line tool head). Each one is checked against the six
    /// fonts by `scripts/check-glyph-coverage.py` and listed in the README's coverage table. A
    /// new glyph makes this test fail until it is re-checked and added here.
    const VERIFIED_COVERED: &str = "»│━─×▖▘▝▗↑↓✓•╭╰…▄▀·";

    /// The documented exceptions to the coverage rule (ADR 0027 and ADR 0035's
    /// 2026-10-16 notes): the arc and the pulse were picked after review, braille is
    /// an opt-in, and the fold marker pair `◼`/`◻` is the same kind of choice (four of the six
    /// reference fonts lack them; they were chosen after review), so these may fall
    /// back to another font. Kept separate so the rest of the check is not loosened.
    const DOCUMENTED_EXCEPTIONS: &str = "◼◻▨◜◝◞◟⏺⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

    fn app_with_glyphs(name: &str) -> App {
        let mut app = App::new("mock", "s");
        let config = radian_host::config::TuiFooterConfig {
            glyphs: Some(name.to_string()),
            ..Default::default()
        };
        app.apply_footer(&config);
        app
    }

    fn bare_item(id: &str) -> Item {
        Item {
            id: id.to_string(),
            icon: None,
            symbol_style: None,
            format: None,
            style: None,
            priority: components::priority(id),
            thresholds: Vec::new(),
            params: Vec::new(),
            disabled: false,
            unknown: false,
        }
    }

    /// The brand mark is per glyph set (ADR 0027, plus the emoji set from
    /// ): Nerd Font, unicode `R`, ascii `rad`, emoji bolt; `㎭`
    /// is gone.
    #[test]
    fn the_brand_mark_is_per_glyph_set() {
        for (name, expected) in [
            ("unicode", "R"),
            ("nerd", "\u{f0c20}"),
            ("ascii", "rad"),
            ("emoji", "⚡"),
        ] {
            let app = app_with_glyphs(name);
            let item = bare_item("brand");
            let component = components::builtin("brand").expect("the brand component");
            let data = component.data(&app, &item).expect("brand data");
            let value = data
                .values
                .iter()
                .find(|(key, _)| *key == "brand")
                .expect("the brand value")
                .1
                .clone();
            assert_eq!(value, expected, "the {name} brand mark");
        }
    }

    /// The arc frames and the pulse glyph are the only coverage exceptions, and they are
    /// exactly the ones the ADR documents (revisions 9/10).
    #[test]
    fn the_spinner_exceptions_are_documented() {
        for glyph in crate::spinner::ARC
            .iter()
            .chain(crate::spinner::BRAILLE.iter())
        {
            for ch in glyph.chars() {
                assert!(DOCUMENTED_EXCEPTIONS.contains(ch), "{glyph}");
            }
        }
        assert!(DOCUMENTED_EXCEPTIONS.contains('⏺'));
        // The quadrant frames are the covered optional set.
        for glyph in crate::spinner::QUADRANT {
            assert!(VERIFIED_COVERED.contains(glyph), "{glyph}");
        }
    }

    /// The fold marker pair `◼`/`◻` is a documented exception (ADR 0035's 2026-10-16 note): the
    /// Chosen knowing four of the six reference fonts lack it. The `ascii` tier and the
    /// one-line head marker are ASCII and stay covered.
    #[test]
    fn the_fold_markers_are_documented_exceptions() {
        for glyphs in [Glyphs::Unicode, Glyphs::Nerd] {
            for marker in [glyphs.fold_marker(), glyphs.open_marker()] {
                for ch in marker.chars().filter(|ch| !ch.is_whitespace()) {
                    assert!(DOCUMENTED_EXCEPTIONS.contains(ch), "{marker:?}");
                }
            }
        }
        for marker in [Glyphs::Ascii.fold_marker(), Glyphs::Ascii.open_marker()] {
            assert!(marker.is_ascii(), "{marker:?}");
        }
        // The one-line tool head marker stays covered (ADR 0028).
        assert!(VERIFIED_COVERED.contains('•'));
    }

    /// The unicode tier only draws glyphs the six checked fonts all have. Every item glyph
    /// (from the built-in presets' TOML), the spinner frames, the separators, the context bar and
    /// the auto marker are checked.
    #[test]
    fn unicode_tier_glyphs_are_all_verified_covered() {
        let mut glyphs: Vec<char> = Vec::new();
        for preset in [Preset::Deck, Preset::Full, Preset::Pi] {
            // The presets are TOML now (ADR 0027); the parsed rows are
            // what the renderer reads, so the coverage check reads them too.
            let config = presets::preset_config(preset);
            for row in &config.rows {
                for item in row.left.iter().chain(row.right.iter()) {
                    let glyph = components::builtin(&item.id).map_or("", |component| {
                        components::glyph_of(component, Glyphs::Unicode)
                    });
                    glyphs.extend(glyph.chars());
                    if let Some(format) = &item.format {
                        // The default formats carry no glyph beyond ASCII; keep them honest.
                        glyphs.extend(format.chars());
                    }
                }
                glyphs.extend(row.separator.as_deref().unwrap_or(" · ").chars());
            }
        }
        glyphs.extend(brand_mark(Glyphs::Unicode).chars());
        glyphs.extend(Glyphs::Unicode.tool_status().iter().flat_map(|s| s.chars()));
        // The spinner's covered sets (quadrant/braille/ascii); `arc` and `pulse` are the
        // documented exceptions, checked separately below.
        for glyph in crate::spinner::glyphs() {
            for ch in glyph.chars() {
                if DOCUMENTED_EXCEPTIONS.contains(ch) {
                    continue;
                }
                glyphs.push(ch);
            }
        }
        glyphs.extend("━─".chars());
        // The status line's truncation marker (ADR 0027 item 2: `…`).
        glyphs.extend("…".chars());
        glyphs.extend(" ~".chars());
        for ch in glyphs {
            if ch.is_ascii() || ch == ' ' {
                continue;
            }
            assert!(
                VERIFIED_COVERED.contains(ch),
                "{ch:?} (U+{:04X}) is not in the verified-covered set; re-run \
                 scripts/check-glyph-coverage.py and update the README",
                ch as u32
            );
        }
    }

    /// ADR 0027: one registry, one entry per built-in component, every id
    /// unique and findable, and every component declares at least one placeholder (the old
    /// `item_is_known` rule). The set is the documented C2 items plus `git_branch`/`worktree`/
    /// `status`, which are ordinary components now.
    #[test]
    fn the_registry_lists_every_builtin_once() {
        let ids = components::ids();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "duplicate component id: {ids:?}");
        for id in &ids {
            let component = components::builtin(id).expect("findable by its own id");
            assert_eq!(component.id(), *id);
            assert!(
                !components::placeholders(id).unwrap().is_empty(),
                "{id} declares no placeholders"
            );
        }
        for expected in [
            "brand",
            "host",
            "model",
            "reasoning",
            "tier",
            "cwd",
            "git_branch",
            "worktree",
            "status",
            "session",
            "context",
            "usage",
            "tokens",
            "speed",
            "elapsed",
            "cost",
            "pending",
            "jobs",
            "mcp",
            "scroll",
        ] {
            assert!(
                ids.contains(&expected),
                "the registry is missing {expected}"
            );
        }
    }

    /// ADR 0027: one component per file. Every registered id has its own
    /// `src/footer/components/<id>.rs`, so a new built-in cannot be registered without its file
    /// (a file with no registry entry is the registry test above's job); the C2 item list and
    /// the old id tables were all one-for-one with these files, and a `git_branch`/`worktree`/
    /// `status` file is a component like any other (no contributor special case).
    #[test]
    fn every_registry_id_has_a_component_file() {
        for id in components::ids() {
            let path = format!(
                "{}/src/footer/components/{id}.rs",
                env!("CARGO_MANIFEST_DIR")
            );
            assert!(
                std::path::Path::new(&path).is_file(),
                "{id} is registered but has no component file at {path}"
            );
        }
    }

    /// ADR 0027: the registry must reproduce the pre-refactor per-id tables
    /// from `631c8dd` exactly. The refactor dropped the `item_priority` overrides of `brand`,
    /// `model` and `host`, so they silently fell back to the generic 50; this pins **every**
    /// built-in id's default priority, so a missing override fails here instead of only changing a
    /// narrow-terminal truncation order. Slice ③ merged `context_bar` into `context` (decision 6),
    /// so the table has one row fewer than `631c8dd`'s.
    #[test]
    fn the_registry_matches_the_pre_refactor_priority_table() {
        // The `631c8dd` `item_priority` table, plus `usage` (ADR 0027):
        // it replaces the usage contribution id, which the table never named and which
        // therefore kept the generic 50. Minus `context_bar` (decision 6 merged it into
        // `context`, keeping its 85), plus `jobs` (the 2026-10-17 note's component: a running
        // job is as worth keeping as a pending request, so 70 like `pending`), plus `queued`
        // (a waiting followup is as worth keeping as a pending request, so 70 too), plus
        // `loop` (a running loop is worth keeping like a waiting followup, so 70 too).
        let expected: [(&str, i64); 22] = [
            ("brand", 92),
            ("host", 78),
            ("model", 90),
            ("reasoning", 60),
            ("tier", 75),
            ("cwd", 80),
            ("git_branch", 70),
            ("worktree", 70),
            ("status", 45),
            ("session", 65),
            ("context", 85),
            ("usage", 50),
            ("tokens", 50),
            ("speed", 55),
            ("elapsed", 35),
            ("cost", 40),
            ("pending", 70),
            ("queued", 70),
            ("loop", 70),
            ("jobs", 70),
            ("mcp", 55),
            ("scroll", 20),
        ];
        assert_eq!(
            components::ids().len(),
            expected.len(),
            "registry/table size"
        );
        for (id, priority) in expected {
            assert_eq!(components::priority(id), priority, "priority for {id}");
        }
        // An id no component claims keeps the table's `_` arm.
        assert_eq!(components::priority("no.such.id"), 50);
    }

    /// ADR 0027: the registry's `placeholders` must match the `631c8dd`
    /// `item_placeholders` table, id for id. Slice ③ merged `context_bar` into `context`
    /// (decision 6): `context` keeps the shared five placeholders, the old row is gone.
    #[test]
    fn the_registry_matches_the_pre_refactor_placeholder_table() {
        // Plus `usage` (ADR 0027), whose one placeholder is the whole
        // line, like a `command` item's `{value}`, plus `queued` (one `{count}`, like
        // `pending`). plus `loop` (its `{count}`, `{every}` and `{next}` states).
        let expected: [(&str, &[&str]); 22] = [
            ("brand", &["brand"]),
            ("host", &["host"]),
            ("model", &["display_name", "id", "provider"]),
            ("reasoning", &["level"]),
            ("tier", &["tier"]),
            (
                "cwd",
                &["path", "path_home", "basename", "repo", "worktree"],
            ),
            ("git_branch", &["branch", "dirty", "modified", "untracked"]),
            ("worktree", &["worktree"]),
            ("status", &["status"]),
            ("session", &["id"]),
            ("context", &["bar", "percent", "used", "budget", "auto"]),
            ("usage", &["value"]),
            (
                "tokens",
                &["in", "out", "cache", "cache_write", "in_arrow", "out_arrow"],
            ),
            ("speed", &["speed"]),
            ("elapsed", &["elapsed"]),
            ("cost", &["usd"]),
            ("pending", &["count"]),
            ("queued", &["count"]),
            ("loop", &["count", "every", "next"]),
            ("jobs", &["count", "age"]),
            ("mcp", &["mcp"]),
            ("scroll", &["position", "mode"]),
        ];
        assert_eq!(
            components::ids().len(),
            expected.len(),
            "registry/table size"
        );
        for (id, placeholders) in expected {
            assert_eq!(
                components::placeholders(id),
                Some(placeholders),
                "placeholders for {id}"
            );
        }
    }

    /// ADR 0027: the registry's default role must match the `631c8dd`
    /// `default_role` table. The `nerd` glyph set with no `tui.brand` is exactly the case the old
    /// plain values were reached in (`brand` is the one conditional: `text` here, the accent role
    /// under `unicode` or a configured brand, ADR 0027).
    #[test]
    fn the_registry_matches_the_pre_refactor_role_table() {
        let app = app_with_glyphs("nerd");
        let expected: [(&str, Option<Role>); 22] = [
            ("brand", Some(Role::Text)),
            ("host", Some(Role::NoticeBody)),
            ("model", Some(Role::Model)),
            ("reasoning", None),
            ("tier", Some(Role::Tier)),
            ("cwd", None),
            ("git_branch", None),
            ("worktree", None),
            ("status", None),
            ("session", None),
            ("context", Some(Role::Budget)),
            // The snapshot's style is the data's own (fresh `text`, stale `dim`), like a
            // contribution's role before the refactor.
            ("usage", None),
            ("tokens", Some(Role::Usage)),
            ("speed", Some(Role::Usage)),
            ("elapsed", Some(Role::Truncation)),
            ("cost", None),
            ("pending", Some(Role::TurnRunning)),
            ("queued", Some(Role::TurnRunning)),
            ("loop", Some(Role::TurnRunning)),
            ("jobs", None),
            ("mcp", Some(Role::Mcp)),
            ("scroll", None),
        ];
        for (id, role) in expected {
            let component = components::builtin(id).expect("registered");
            assert_eq!(component.role(&app), role, "default role for {id}");
        }
    }

    /// ADR 0027: the registry's default thresholds must match the `631c8dd`
    /// `default_thresholds` table (`context`/`cost` at 70/90; every other id empty). Slice ③
    /// merged `context_bar` into `context`, so its row is gone; the pair it shared survives on
    /// `context`.
    #[test]
    fn the_registry_matches_the_pre_refactor_threshold_table() {
        let numeric = vec![
            (70, ItemStyle::Role(Role::BudgetOver)),
            (90, ItemStyle::Role(Role::ToolResultError)),
        ];
        for id in components::ids() {
            let expected = if matches!(id, "context" | "cost") {
                numeric.clone()
            } else {
                Vec::new()
            };
            assert_eq!(components::thresholds(id), expected, "thresholds for {id}");
        }
    }

    /// ADR 0027: the registry's default glyph in the three selectable sets
    /// must match the `631c8dd` `glyph` table; decision 7 adds the fourth, emoji column, drawn
    /// by the system emoji font (the six-font coverage rule does not apply to it). Slice ③
    /// item 2 only removes `context_bar`, merged into `context` by decision 6, whose row had
    /// an empty icon in every set.
    #[test]
    fn the_registry_matches_the_pre_refactor_glyph_table() {
        let expected: [(&str, [&str; 4]); 22] = [
            ("brand", ["", "", "", ""]),
            ("host", ["@ ", "\u{f109} ", "host ", "💻 "]),
            ("model", ["", "", "", ""]),
            ("reasoning", ["* ", "\u{f0eb} ", "* ", "🧠 "]),
            ("tier", ["", "", "", ""]),
            ("cwd", ["", "", "", ""]),
            ("git_branch", ["", "\u{e0a0} ", "", ""]),
            ("worktree", ["", "", "", ""]),
            ("status", ["", "", "", ""]),
            ("session", ["# ", "", "# ", "# "]),
            ("context", ["", "\u{f035b} ", "[", ""]),
            ("usage", ["", "", "", ""]),
            ("tokens", ["", "", "", ""]),
            ("speed", ["\u{bb} ", "\u{f0e7} ", "spd ", "🚀 "]),
            ("elapsed", ["", "\u{f017} ", "t ", "⏱ "]),
            ("cost", ["", "", "", ""]),
            ("pending", ["", "", "", ""]),
            ("queued", ["", "", "", ""]),
            ("loop", ["", "", "", ""]),
            ("jobs", ["", "", "", ""]),
            ("mcp", ["", "", "", ""]),
            ("scroll", ["", "", "", ""]),
        ];
        assert_eq!(
            components::ids().len(),
            expected.len(),
            "registry/table size"
        );
        for (id, glyphs) in expected {
            let component = components::builtin(id).expect("registered");
            assert_eq!(
                component.glyphs(),
                glyphs,
                "default glyphs for {id} (unicode/nerd/ascii/emoji)"
            );
        }
    }

    /// ADR 0027 (slice ⑤ item 4): a parse error in a built-in preset
    /// is a test failure, never a runtime surprise. The loader parses all four TOML files with
    /// the user's config parser (`preset_config` panics otherwise, and this test fails first);
    /// each preset carries its documented rows: deck 3, full 2, pi 2, classic none.
    #[test]
    fn all_four_built_in_presets_load_their_rows() {
        let mut warnings = Vec::new();
        assert_eq!(preset_footer_rows(Preset::Deck, &mut warnings).len(), 3);
        assert_eq!(preset_footer_rows(Preset::Full, &mut warnings).len(), 2);
        assert_eq!(preset_footer_rows(Preset::Pi, &mut warnings).len(), 2);
        assert_eq!(preset_footer_rows(Preset::Classic, &mut warnings).len(), 0);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// ADR 0027: the draw path has no item-id tables. The pre-refactor
    /// dispatch (`item_placeholders`, `item_priority`, `default_thresholds`, the
    /// `CONTRIBUTION_ITEMS` list, …) is gone; the renderer reaches every item through
    /// `components::lookup`. The positive control pins that the lookup call is there, so this
    /// test cannot pass because dispatch moved nowhere. The draw path lives in
    /// `footer/draw.rs` (fix round 1 split); `footer.rs` itself keeps the check so no id
    /// table creeps back into either file.
    #[test]
    fn the_render_path_has_no_item_id_tables() {
        let draw =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/footer/draw.rs"))
                .expect("footer/draw.rs is readable");
        let footer = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/footer.rs"))
            .expect("footer.rs is readable");
        // Only the non-test code: the test module names the removed functions on purpose.
        let code = draw
            .split_once("\n#[cfg(test)]")
            .map_or(draw.as_str(), |(before, _)| before);
        let footer_code = footer
            .split_once("\n#[cfg(test)]")
            .map_or(footer.as_str(), |(before, _)| before);
        for forbidden in [
            "fn item_placeholders",
            "fn item_priority",
            "fn default_thresholds",
            "fn default_role",
            "fn item_data",
            "fn contribution_item",
            "const CONTRIBUTION_ITEMS",
            "fn item_is_known",
        ] {
            for (name, source) in [("footer/draw.rs", code), ("footer.rs", footer_code)] {
                assert!(
                    !source.contains(forbidden),
                    "{name}: {forbidden} is back: the render path must not special-case item ids"
                );
            }
        }
        assert!(
            code.contains("components::lookup("),
            "the renderer dispatches through the component registry"
        );
    }

    /// ADR 0027 slice ②'s acceptance: no source file under `src/footer*` names a
    /// provider (the acceptance is a text search for a provider's name over exactly these
    /// files). The `usage` component draws whatever snapshot the store holds, and which
    /// providers publish one — and under which ids — is the descriptors' business, not the
    /// footer's. The needles come from the descriptors themselves, so this test's own text
    /// need not spell any of them.
    #[test]
    fn no_footer_source_names_a_provider() {
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("a readable directory") {
                let path = entry.expect("an entry").path();
                if path.is_dir() {
                    collect(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = vec![src.join("footer.rs")];
        collect(&src.join("footer"), &mut sources);
        assert!(sources.len() > 1, "the footer module's files are found");
        let mut needles: Vec<String> = Vec::new();
        for descriptor in radian_host::descriptors() {
            needles.push(descriptor.id.to_owned());
            if let Some(radian_host::UsageSource::Endpoint(endpoint)) = &descriptor.usage {
                needles.extend(endpoint.legacy_item_ids.iter().map(|id| (*id).to_owned()));
            }
        }
        assert!(
            needles.iter().any(|needle| needle.contains('.')),
            "the search covers the old item ids too"
        );
        for path in sources {
            let text = std::fs::read_to_string(&path).expect("a readable source");
            for needle in &needles {
                assert!(
                    !text.contains(needle.as_str()),
                    "{} names {needle:?}; the footer must not",
                    path.display()
                );
            }
        }
    }

    /// ADR 0027: an arbitrary id a contributor publishes resolves through
    /// the registry's dynamic fallback, not an id list. `git_branch`/`worktree`/`status` are
    /// built-in components; a brand-new id resolves only once the store holds it.
    #[test]
    fn an_arbitrary_store_id_resolves_through_the_registry() {
        let app = App::new("mock", "s");
        assert!(
            components::builtin("custom.thing").is_none(),
            "no built-in claims an arbitrary id"
        );
        assert!(
            components::lookup(&app, "custom.thing").is_none(),
            "no value yet: nothing to resolve"
        );
        app.contributions.write(
            "custom.thing",
            vec![radian_host::Segment::new(
                "hi",
                radian_host::SegmentRole::Normal,
            )],
        );
        assert!(
            components::lookup(&app, "custom.thing").is_some(),
            "the store value resolves through the dynamic component"
        );
    }

    /// Every `Glyphs::ALL` entry round-trips through `parse(as_str())`: `ALL` is the single
    /// source, so a new set without a spelling fails here.
    #[test]
    fn every_glyph_set_round_trips_through_parse() {
        for set in Glyphs::ALL {
            assert_eq!(Glyphs::parse(set.as_str()), Some(*set), "{set:?}");
        }
    }

    /// Every `ContextLine::ALL` entry round-trips through `parse(as_str())`.
    #[test]
    fn every_context_line_round_trips_through_parse() {
        for line in ContextLine::ALL {
            assert_eq!(ContextLine::parse(line.as_str()), Some(*line), "{line:?}");
        }
    }
}
