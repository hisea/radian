//! The footer's rows and built-in items (ADR 0027 C2, slice 1).
//!
//! The footer is rows of `left`/`right` item groups. Each item renders text from the session's
//! facts (the projection, `App`'s startup facts) with its own placeholders, glyph and theme
//! role. **The draw path does no I/O**: `cwd` and `git_branch` are startup facts refreshed by
//! the host ([`crate::app::App::set_git_branch`]), and nothing here runs git. Colors come only
//! from theme roles (ADR 0029). An unknown item id, placeholder or parameter is shown raw
//! with one warning and never blocks startup (the ADR 0008 `tui.*` rule).

use std::borrow::Cow;

use radian_host::SegmentRole;
use radian_host::config::{TuiFooterConfig, TuiFooterItemConfig};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::theme::Role;

mod components;

use components::{Component, ItemData};

/// The brand mark for a glyph set (ADR 0027): the Nerd Font `md-alpha_r_circle_
/// outline`, the plain bold-accent `R` (the same font as the body, so it sits on the baseline),
/// or the ASCII word `rad`. `㎭` (U+33AD) is East-Asian-Wide and its fallback glyph sits off the
/// baseline, so it is no longer used.
pub fn brand_mark(glyphs: Glyphs) -> &'static str {
    match glyphs {
        Glyphs::Nerd => "\u{f0c20}",
        Glyphs::Unicode => "R",
        Glyphs::Ascii => "rad",
    }
}

/// How the built-in items draw their icons (ADR 0027 C2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Glyphs {
    /// Unicode box/geometric glyphs (the default).
    #[default]
    Unicode,
    /// Nerd Font code points (the owner's pi footer).
    Nerd,
    /// Pure ASCII.
    Ascii,
}

impl Glyphs {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "unicode" => Some(Self::Unicode),
            "nerd" => Some(Self::Nerd),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }

    /// The card frame's characters (ADR 0028 rule 2): unicode box drawing for the
    /// unicode and nerd sets, `+ - |` for ascii.
    pub fn frame(self) -> FrameGlyphs {
        match self {
            Self::Unicode | Self::Nerd => FrameGlyphs {
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
    /// The unicode set is covered by all six checked fonts (ADR 0027 and 6): omp's
    /// `⟳`/`✔`/`✘` are not, so `~`/`✓`/`×` stand in. The Nerd set uses the Font Awesome
    /// codepoints; the ASCII set is plain.
    pub fn tool_status(self) -> [&'static str; 3] {
        match self {
            Self::Unicode => ["~", "✓", "×"],
            Self::Nerd => ["\u{f110}", "\u{f00c}", "\u{f00d}"],
            Self::Ascii => ["*", "+", "x"],
        }
    }

    /// The token arrows: the unicode tier uses `↑`/`↓` (revision 6 item 2; both are in all six
    /// checked fonts), the other tiers keep `^`/`v`.
    pub fn token_arrows(self) -> (&'static str, &'static str) {
        match self {
            Self::Unicode => ("↑", "↓"),
            Self::Nerd | Self::Ascii => ("^", "v"),
        }
    }

    /// The marker column's **folded** spelling (ADR 0035's 2026-10-16 "折叠标记统一" note): the
    /// thinking channel's folded one-liner and an expandable tool head that shows only its head
    /// both take it. `◼ ` on the unicode and nerd tiers, `+ ` under `ascii`. `◼`/`◻` miss four
    /// of the six reference fonts: the owner chose the pair anyway, a documented exception like
    /// the ask record's `◉`/`○`.
    pub fn fold_marker(self) -> &'static str {
        match self {
            Self::Unicode | Self::Nerd => "◼ ",
            Self::Ascii => "+ ",
        }
    }

    /// The marker column's **expanded** spelling (same note): a body drawn under the head (the
    /// `errors` preview, an error body, `Ctrl-O`/search expansion, the expanded thinking
    /// channel). `◻ ` on the unicode and nerd tiers, `- ` under `ascii`.
    pub fn open_marker(self) -> &'static str {
        match self {
            Self::Unicode | Self::Nerd => "◻ ",
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
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "off" => Some(Self::Off),
            "percentage" => Some(Self::Percentage),
            _ => None,
        }
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

/// Which footer the session shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// The default (ADR 0027): three rows — the opencode-go usage widget (absent
    /// without data), the identity row, and the numbers row.
    #[default]
    Deck,
    /// The revision-1 two-row preset: the identity row and the numbers row.
    Full,
    /// The earlier two-row preset: model/cwd/branch/context bar/reasoning, then tokens/cost.
    Pi,
    /// Today's one status line, byte-for-byte (drawn by the renderer).
    Classic,
}

impl Preset {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "deck" => Some(Self::Deck),
            "full" => Some(Self::Full),
            "pi" => Some(Self::Pi),
            "classic" => Some(Self::Classic),
            _ => None,
        }
    }
}

/// One item's style (ADR 0027): a theme role (follows the theme) or a fixed
/// starship-style color (does not). The theme stays the only color source for a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemStyle {
    Role(Role),
    Fixed(Style),
}

impl ItemStyle {
    fn resolve(&self, app: &App) -> Style {
        match self {
            ItemStyle::Role(role) => role_style(app, *role),
            ItemStyle::Fixed(style) => *style,
        }
    }
}

/// One resolved item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// The configured id as written (an unknown id is shown raw).
    pub(crate) id: String,
    /// The item's own `icon`/`symbol`, else the component's default glyph for the id.
    pub(crate) icon: Option<String>,
    /// The symbol's own style, when it differs from the text's (revision 3).
    pub(crate) symbol_style: Option<ItemStyle>,
    pub(crate) format: Option<String>,
    pub(crate) style: Option<ItemStyle>,
    pub(crate) priority: i64,
    pub(crate) thresholds: Vec<(i64, ItemStyle)>,
    /// The component's own parameters (`context_bar`'s `width`/`fill`/`empty`, `status`'s
    /// `keys`), read by its `data`.
    pub(crate) params: Vec<(String, String)>,
    /// `disabled = true` in `[tui.footer.items.<id>]` hides the item everywhere.
    pub(crate) disabled: bool,
    /// No built-in component claims this id *and* the user wrote it: an unresolved id shows its
    /// id text raw (ADR 0008). A preset's id (or one a contributor later publishes) is not
    /// unknown: it simply hides until it has data.
    pub(crate) unknown: bool,
}

/// A starship-style style (ADR 0027): space-separated modifiers and colors. A
/// named color (`red`, `bright_blue`), `#rrggbb`, or a 0–255 number; `bold`/`italic`/`dimmed`
/// (and `underline`). The first color is the foreground, a second the background. An empty
/// string is the default style; an unknown token is an error (a config error, not ignored).
fn parse_starship_style(text: &str) -> Result<Style, String> {
    let mut style = Style::default();
    let mut colors = 0;
    for token in text.split_whitespace() {
        match token {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "dimmed" => style = style.add_modifier(Modifier::DIM),
            "underline" => style = style.add_modifier(Modifier::UNDERLINED),
            _ => {
                let color = parse_color(token).ok_or_else(|| format!("unknown color {token:?}"))?;
                colors += 1;
                match colors {
                    1 => style = style.fg(color),
                    2 => style = style.bg(color),
                    _ => return Err(format!("more than two colors in {text:?}")),
                }
            }
        }
    }
    Ok(style)
}

/// A named ANSI color, `#rrggbb`, or a 0–255 index (starship's color vocabulary).
fn parse_color(token: &str) -> Option<Color> {
    if let Some(hex) = token.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let byte = |range: std::ops::Range<usize>| u8::from_str_radix(hex.get(range)?, 16).ok();
        return Some(Color::Rgb(byte(0..2)?, byte(2..4)?, byte(4..6)?));
    }
    if let Ok(index) = token.parse::<u8>() {
        return Some(Color::Indexed(index));
    }
    Some(match token {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "purple" | "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::Gray,
        "bright_black" => Color::DarkGray,
        "bright_red" => Color::LightRed,
        "bright_green" => Color::LightGreen,
        "bright_yellow" => Color::LightYellow,
        "bright_blue" => Color::LightBlue,
        "bright_purple" | "bright_magenta" => Color::LightMagenta,
        "bright_cyan" => Color::LightCyan,
        "bright_white" => Color::White,
        _ => return None,
    })
}

/// A `style` value: a theme role (or pi alias) first, else a starship color. An error is the
/// caller's warning.
fn parse_item_style(text: &str) -> Result<ItemStyle, String> {
    if let Some(role) = parse_style(text) {
        return Ok(ItemStyle::Role(role));
    }
    parse_starship_style(text).map(ItemStyle::Fixed)
}

/// One configured row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    left: Vec<Item>,
    right: Vec<Item>,
    separator: String,
    /// The separator's style (ADR 0027): the `footer_separator` role by default,
    /// `[tui.footer] separator_style` can make it a color.
    separator_style: ItemStyle,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
            separator: String::new(),
            separator_style: ItemStyle::Role(Role::FooterSeparator),
        }
    }
}

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
                    "tui.footer.glyphs = {name:?} is not one of unicode|nerd|ascii; using unicode"
                ));
                Glyphs::Unicode
            }),
        };
        let separator = config
            .separator
            .clone()
            .unwrap_or_else(|| " │ ".to_string());
        let separator_style = match config.separator_style.as_deref() {
            None => ItemStyle::Role(Role::FooterSeparator),
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
        let rows: Vec<Row> = if config.rows.is_empty() {
            // The built-in preset, then the per-field `items` config on top.
            let mut rows = preset_rows(preset, glyphs);
            for row in &mut rows {
                for item in row.left.iter_mut().chain(row.right.iter_mut()) {
                    if let Some(field) = config.items.get(&item.id) {
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
            config
                .rows
                .iter()
                .map(|row| Row {
                    left: row
                        .left
                        .iter()
                        .filter_map(|item| {
                            resolve_item(item, &config.items, &command_ids, &mut warnings)
                        })
                        .collect(),
                    right: row
                        .right
                        .iter()
                        .filter_map(|item| {
                            resolve_item(item, &config.items, &command_ids, &mut warnings)
                        })
                        .collect(),
                    separator: row.separator.clone().unwrap_or_else(|| separator.clone()),
                    separator_style: separator_style.clone(),
                })
                .collect()
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

    /// The configured glyph set (the renderer reads it for other chrome too).
    pub fn glyphs(&self) -> Glyphs {
        self.glyphs
    }

    /// The footer's lines at `width`. The `classic` preset is the renderer's (it draws the
    /// pre-ADR one-line status bar); `pi` builds its rows here.
    pub fn lines(&self, app: &App, width: usize) -> Vec<Line<'static>> {
        let rows: Cow<'_, [Row]> = if self.rows.is_empty() {
            Cow::Owned(preset_rows(self.preset, self.glyphs))
        } else {
            Cow::Borrowed(&self.rows)
        };
        // A row with no data renders empty (the deck's usage row, revision 3): drop it so it
        // never leaves a blank line.
        rows.iter()
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
}

/// The built-in rows for a preset (ADR 0027 C2 and revision 1; a configured `rows` overrides
/// them entirely). `classic` is drawn by the renderer, so its rows are unused.
fn preset_rows(preset: Preset, glyphs: Glyphs) -> Vec<Row> {
    let (token_in, token_out) = glyphs.token_arrows();
    let item = |id: &str| Item {
        id: id.to_string(),
        icon: None,
        symbol_style: None,
        format: None,
        style: None,
        priority: components::priority(id),
        thresholds: components::thresholds(id),
        params: Vec::new(),
        disabled: false,
        unknown: false,
    };
    let with_format = |id: &str, format: &str| Item {
        format: Some(format.to_string()),
        ..item(id)
    };
    // A preset row with no symbol at all (revision 3's numbers row has none).
    let no_symbol = |id: &str, format: &str| Item {
        icon: Some(String::new()),
        format: Some(format.to_string()),
        ..item(id)
    };
    let row = |left: Vec<Item>, right: Vec<Item>| Row {
        left,
        right,
        separator: " │ ".to_string(),
        separator_style: ItemStyle::Role(Role::FooterSeparator),
    };
    match preset {
        Preset::Deck => vec![
            // Row 1: the opencode-go usage widget (ADR 0027 C4). With no data every item is
            // empty and `lines` drops the whole row, so it never leaves a blank line.
            row(vec![item("opencode-go.usage")], Vec::new()),
            // Row 2: identity, with the context bar and reasoning on the right.
            row(
                vec![
                    item("model"),
                    with_format("cwd", "{repo}"),
                    with_format("git_branch", "{branch}{dirty}"),
                    item("worktree"),
                    item("tasks"),
                ],
                vec![item("context_bar"), item("reasoning")],
            ),
            // Row 3: the numbers, right-aligned, single spaces (revision 3's sample
            // `↑15M ↓2.6M R1962M $14.510 41.3%/1.0M`).
            Row {
                separator: " ".to_string(),
                ..row(
                    Vec::new(),
                    vec![
                        with_format(
                            "tokens",
                            &format!("{token_in}{{in}} {token_out}{{out}} R{{cache}}"),
                        ),
                        item("cost"),
                        no_symbol("context", "{percent}%/{budget}"),
                    ],
                )
            },
        ],
        Preset::Full => vec![
            Row {
                // The identity row (ADR 0027).
                left: vec![
                    item("brand"),
                    item("host"),
                    item("model"),
                    item("reasoning"),
                    with_format("cwd", "{repo}"),
                    with_format("git_branch", "{branch}{dirty}"),
                    item("session"),
                ],
                right: Vec::new(),
                separator: " │ ".to_string(),
                separator_style: ItemStyle::Role(Role::FooterSeparator),
            },
            Row {
                // The numbers row: tokens (in/out/cache), speed, cost, context (share/window
                // with the auto-compaction marker), elapsed.
                left: vec![
                    with_format(
                        "tokens",
                        &format!("{token_in}{{in}} {token_out}{{out}} {{cache}}"),
                    ),
                    item("speed"),
                    item("cost"),
                    with_format("context", "{percent}%/{budget}{auto}"),
                    item("elapsed"),
                ],
                right: Vec::new(),
                separator: " │ ".to_string(),
                separator_style: ItemStyle::Role(Role::FooterSeparator),
            },
        ],
        Preset::Pi => vec![
            Row {
                left: vec![
                    item("model"),
                    item("cwd"),
                    item("git_branch"),
                    item("context_bar"),
                ],
                right: vec![item("reasoning")],
                separator: " │ ".to_string(),
                separator_style: ItemStyle::Role(Role::FooterSeparator),
            },
            Row {
                left: vec![item("tokens"), item("cost")],
                right: Vec::new(),
                separator: " │ ".to_string(),
                separator_style: ItemStyle::Role(Role::FooterSeparator),
            },
        ],
        Preset::Classic => Vec::new(),
    }
}

/// Whether a parsed item carries no options of its own (a bare id in a row). A bare id takes
/// its options from `[tui.footer.items.<id>]`; an inline table does not (revision 3).
fn is_bare(config: &TuiFooterItemConfig) -> bool {
    config.icon.is_none()
        && config.symbol_style.is_none()
        && config.format.is_none()
        && config.style.is_none()
        && config.priority.is_none()
        && config.thresholds.is_empty()
        && config.params.is_empty()
        && !config.disabled
}

/// Whether a configured item carries a `command` (ADR 0027): the id is an
/// out-of-process footer component.
fn is_command_item(config: &TuiFooterItemConfig) -> bool {
    config.params.iter().any(|(key, _)| key == "command")
}

/// Resolves one configured item, recording its warnings. `None` when it is disabled.
fn resolve_item(
    config: &TuiFooterItemConfig,
    items: &std::collections::BTreeMap<String, TuiFooterItemConfig>,
    command_ids: &std::collections::BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Option<Item> {
    // A bare id takes the per-field `items` config; an inline table is used as written.
    let effective = if is_bare(config) {
        items.get(&config.id).unwrap_or(config)
    } else {
        config
    };
    if effective.disabled {
        return None;
    }
    let id = effective.id.clone();
    // A `command` id is known (its producer publishes it); an ordinary id no built-in claims is
    // shown raw until a contributor publishes it, with one warning.
    let command = command_ids.contains(&id);
    if !command && components::builtin(&id).is_none() {
        warnings.push(format!(
            "tui.footer: unknown item {id:?}; showing it as written unless a contributor \
             publishes it"
        ));
    }
    let style = parse_style_or_warn(effective.style.as_deref(), &id, "style", warnings);
    let symbol_style = parse_style_or_warn(
        effective.symbol_style.as_deref(),
        &id,
        "symbol_style",
        warnings,
    );
    let mut thresholds = Vec::new();
    if effective.thresholds.is_empty() {
        thresholds = components::thresholds(&id);
    } else {
        for (at, name) in &effective.thresholds {
            match parse_item_style(name) {
                Ok(style) => thresholds.push((*at, style)),
                Err(error) => warnings.push(format!(
                    "tui.footer: item {id:?} has a bad threshold style {name:?}: {error}"
                )),
            }
        }
    }
    if let Some(format) = &effective.format {
        validate_format(&id, format, command, warnings);
    }
    Some(Item {
        id: id.clone(),
        icon: effective.icon.clone(),
        symbol_style,
        format: effective.format.clone(),
        style,
        priority: effective
            .priority
            .unwrap_or_else(|| components::priority(&id)),
        thresholds,
        params: effective.params.clone(),
        disabled: false,
        unknown: !command && components::builtin(&id).is_none(),
    })
}

/// Parses a `style`/`symbol_style` value, warning (not failing) on a bad one.
fn parse_style_or_warn(
    value: Option<&str>,
    id: &str,
    field: &str,
    warnings: &mut Vec<String>,
) -> Option<ItemStyle> {
    let name = value?;
    match parse_item_style(name) {
        Ok(style) => Some(style),
        Err(error) => {
            warnings.push(format!(
                "tui.footer: item {id:?} has a bad {field} {name:?}: {error}"
            ));
            None
        }
    }
}

/// Merges `[tui.footer.items.<id>]` into a preset row's item (revision 3): a configured field
/// overrides the preset's.
fn merge_into_item(item: &mut Item, config: &TuiFooterItemConfig, warnings: &mut Vec<String>) {
    if config.disabled {
        item.disabled = true;
    }
    if let Some(icon) = &config.icon {
        item.icon = Some(icon.clone());
    }
    if let Some(symbol_style) = parse_style_or_warn(
        config.symbol_style.as_deref(),
        &item.id,
        "symbol_style",
        warnings,
    ) {
        item.symbol_style = Some(symbol_style);
    }
    if let Some(format) = &config.format {
        validate_format(&item.id, format, false, warnings);
        item.format = Some(format.clone());
    }
    if let Some(style) = parse_style_or_warn(config.style.as_deref(), &item.id, "style", warnings) {
        item.style = Some(style);
    }
    if let Some(priority) = config.priority {
        item.priority = priority;
    }
    if !config.thresholds.is_empty() {
        item.thresholds = config
            .thresholds
            .iter()
            .filter_map(|(at, name)| parse_item_style(name).ok().map(|style| (*at, style)))
            .collect();
    }
    for (key, value) in &config.params {
        item.params.retain(|(name, _)| name != key);
        item.params.push((key.clone(), value.clone()));
    }
}

/// Warns about an unknown placeholder or a bad inline style in a format, once at parse time.
fn validate_format(id: &str, format: &str, command: bool, warnings: &mut Vec<String>) {
    // A command item's data is the store's `{value}` (ADR 0027); every
    // other id takes the component registry's placeholder list.
    let known: &[&str] = if command {
        &["value"]
    } else {
        components::placeholders(id).unwrap_or(&[])
    };
    for placeholder in placeholders(format) {
        if !known.contains(&placeholder.as_str()) {
            warnings.push(format!(
                "tui.footer: item {id:?} has unknown placeholder {{{placeholder}}}; showing \
                 the format as written"
            ));
        }
    }
    for style in inline_styles(format) {
        if let Err(error) = parse_item_style(&style) {
            warnings.push(format!(
                "tui.footer: item {id:?} has a bad inline style ({style:?}): {error}"
            ));
        }
    }
}

/// A style name: a Radian role name (`notice_body`, `budget_over`, …) or pi's common aliases
/// (`dim`, `muted`, `accent`, `warn`, `error`, `ok`, `text`).
fn parse_style(name: &str) -> Option<Role> {
    Role::parse(name).or_else(|| {
        Some(match name {
            "dim" => Role::Truncation,
            "muted" => Role::NoticeBody,
            "accent" => Role::NoticeLabel,
            "warn" => Role::BudgetOver,
            "error" => Role::ToolResultError,
            "ok" => Role::TurnIdle,
            "text" => Role::Text,
            _ => return None,
        })
    })
}

fn placeholders(format: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}

/// The used fraction of the context window, `None` when either the budget or the usage report
/// is unknown (ADR 0027 item 4: the gap gauge and the `context` item share it).
pub(crate) fn context_fraction(session: &radian_host::ProjectionRead<'_>) -> Option<f64> {
    let budget = session.context_window()?;
    let used = session.budget()?.projected_tokens;
    (budget > 0).then(|| used as f64 / budget as f64)
}

/// One rendered item: its styled spans (symbol + text) and its truncation priority.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    spans: Vec<(String, Style)>,
    priority: i64,
}

fn render_row(app: &App, row: &Row, width: usize, context_line: ContextLine) -> Line<'static> {
    let mut left: Vec<Piece> = row
        .left
        .iter()
        .filter_map(|item| render_item(app, item))
        .collect();
    let mut right: Vec<Piece> = row
        .right
        .iter()
        .filter_map(|item| render_item(app, item))
        .collect();
    let separator = row.separator.as_str();
    let separator_style = row.separator_style.resolve(app);

    // Narrow terminals drop the right group first, then the left group from its end; the
    // lowest priority goes first (ties: the rightmost of its group).
    loop {
        let total = group_width(&left, separator)
            + group_width(&right, separator)
            + usize::from(!left.is_empty() && !right.is_empty()) * width_of(separator);
        if total <= width {
            break;
        }
        if drop_lowest(&mut right) || drop_lowest(&mut left) {
            continue;
        }
        break;
    }

    let left_text = join_group(&left, separator, separator_style);
    let right_text = join_group(&right, separator, separator_style);
    let mut spans: Vec<Span> = left_text
        .iter()
        .map(|(text, style)| Span::styled(text.clone(), *style))
        .collect();
    if !right_text.is_empty() {
        let used: usize = spans
            .iter()
            .map(|span| width_of(span.content.as_ref()))
            .sum();
        let gap = width.saturating_sub(used + group_width(&right, separator));
        if gap > 0 {
            // ADR 0027 item 4: the gap can show the context usage (omp's
            // `contextLine = percentage`): the used fraction in the budget role, the rest in
            // the separator (border) role. No known budget: spaces, never a fabricated number.
            // The read is taken only when the fill is on: the projection Mutex is not
            // reentrant, so a call from a path that already holds it would deadlock.
            let fraction = match context_line {
                ContextLine::Percentage => {
                    let session = app.session.projection.read();
                    context_fraction(&session)
                }
                ContextLine::Off => None,
            };
            match fraction {
                Some(fraction) => {
                    let used_cells = ((fraction.clamp(0.0, 1.0)) * gap as f64).round() as usize;
                    let used_cells = used_cells.min(gap);
                    if used_cells > 0 {
                        spans.push(Span::styled(
                            "─".repeat(used_cells),
                            role_style(app, Role::Budget),
                        ));
                    }
                    if gap > used_cells {
                        spans.push(Span::styled(
                            "─".repeat(gap - used_cells),
                            role_style(app, Role::FooterSeparator),
                        ));
                    }
                }
                None => spans.push(Span::raw(" ".repeat(gap))),
            }
        }
        for (text, style) in &right_text {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    Line::from(spans)
}

/// Drops the lowest-priority piece (rightmost on a tie); `false` when the group is empty.
fn drop_lowest(pieces: &mut Vec<Piece>) -> bool {
    if pieces.is_empty() {
        return false;
    }
    let mut index = 0;
    for (i, piece) in pieces.iter().enumerate() {
        if piece.priority <= pieces[index].priority {
            index = i;
        }
    }
    pieces.remove(index);
    true
}

/// The width of one group's text plus its separators.
fn group_width(pieces: &[Piece], separator: &str) -> usize {
    let text: usize = pieces
        .iter()
        .map(|piece| {
            piece
                .spans
                .iter()
                .map(|(text, _)| width_of(text))
                .sum::<usize>()
        })
        .sum();
    let separators = width_of(separator) * pieces.len().saturating_sub(1);
    text + separators
}

/// One group as (text, style) pairs, its items joined by `separator`.
fn join_group(pieces: &[Piece], separator: &str, separator_style: Style) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            out.push((separator.to_string(), separator_style));
        }
        out.extend(piece.spans.iter().cloned());
    }
    out
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// One item's styled spans, or `None` when it has no data (never a fabricated number). The
/// renderer dispatches **only** through the component registry (ADR 0027):
/// an unknown id is shown raw (ADR 0008), and a contributor's id reaches its dynamic component.
fn render_item(app: &App, item: &Item) -> Option<Piece> {
    let Some(resolved) = components::lookup(app, &item.id) else {
        // No component and no contributor value: an id the user wrote shows raw (ADR 0008); a
        // preset's id simply hides until a contributor publishes it.
        if !item.unknown {
            return None;
        }
        let style = item
            .style
            .as_ref()
            .map(|s| s.resolve(app))
            .unwrap_or_default();
        return Some(Piece {
            spans: vec![(item.id.clone(), style)],
            priority: item.priority,
        });
    };
    let component = resolved.get();
    let data = component.data(app, item)?;
    let format = item
        .format
        .clone()
        .unwrap_or_else(|| data.default_format.clone());
    // An unknown placeholder or inline style was warned about at parse time: show it raw.
    let segments = expand_spans(&format, &data).unwrap_or_else(|_| vec![(format.clone(), None)]);
    let default_style = default_style(app, item, &data, component);
    let mut spans: Vec<(String, Style)> = Vec::new();
    let symbol = symbol_for(app, item, &data, component);
    if !symbol.is_empty() {
        let style = item
            .symbol_style
            .as_ref()
            .map(|s| s.resolve(app))
            .unwrap_or(default_style);
        spans.push((symbol, style));
    }
    for (text, inline) in segments {
        let style = inline
            .as_ref()
            .map(|s| s.resolve(app))
            .unwrap_or(default_style);
        spans.push((text, style));
    }
    Some(Piece {
        spans,
        priority: item.priority,
    })
}

/// The item's own `symbol`/`icon` first, then a symbol the data carries (a command's JSON
/// `symbol`, ADR 0027), then the component's default glyph under the
/// footer's set.
fn symbol_for(app: &App, item: &Item, data: &ItemData, component: &dyn Component) -> String {
    item.icon
        .clone()
        .or_else(|| data.symbol.clone())
        .unwrap_or_else(|| components::glyph_of(component, app.footer_glyphs()).to_string())
}

/// The item's style: a style the data itself decided (a contribution's role, a reasoning level),
/// else a threshold, else the configured `style`, else the component's default role (ADR 0027 C2).
fn default_style(app: &App, item: &Item, data: &ItemData, component: &dyn Component) -> Style {
    if let Some(style) = data.fixed_style {
        return style;
    }
    if let Some((_, style)) = item
        .thresholds
        .iter()
        .rev()
        .find(|(at, _)| data.numeric.is_some_and(|value| value as i64 >= *at))
    {
        return style.resolve(app);
    }
    if let Some(style) = &item.style {
        return style.resolve(app);
    }
    component
        .role(app)
        .map(|role| role_style(app, role))
        .unwrap_or_default()
}

fn role_style(app: &App, role: Role) -> Style {
    let mut theme = app.theme.clone();
    *role.style_mut(&mut theme)
}

/// One store item's joined text, for the items that only need the value (`cwd`'s `{repo}`).
fn contribution_text(app: &App, item: &str) -> Option<String> {
    contribution(app, item).map(|(text, _, _)| text)
}

/// One store item as (joined text, first role, stale).
fn contribution(app: &App, item: &str) -> Option<(String, SegmentRole, bool)> {
    let contribution = app.contributions.get(item)?;
    if contribution.segments.is_empty() {
        return None;
    }
    let text: String = contribution
        .segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    Some((text, contribution.segments[0].role, contribution.stale))
}

/// The theme role a contribution's segment role maps to (ADR 0029: colors from the theme).
fn contrib_role(role: SegmentRole) -> Role {
    match role {
        SegmentRole::Normal => Role::Text,
        SegmentRole::Dim => Role::Truncation,
        SegmentRole::Warn => Role::BudgetOver,
        SegmentRole::Ok => Role::TurnIdle,
        SegmentRole::Error => Role::ToolResultError,
        SegmentRole::Accent => Role::NoticeLabel,
    }
}

/// The `{auto}` marker (ADR 0027): shown only while automatic compaction is on. The
/// ASCII set uses a word, the others a glyph (`\u{27f2}` is missing from five of the six fonts,
/// so `~` stands in).
fn auto_marker(app: &App) -> String {
    if app.auto_compact {
        if app.footer_glyphs() == Glyphs::Ascii {
            " auto".to_string()
        } else {
            " ~".to_string()
        }
    } else {
        String::new()
    }
}

/// Expands the `{placeholder}`s in a plain string (used inside a `[text]` segment).
fn expand_placeholders(text: &str, data: &ItemData) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push('{');
            rest = &rest[open + 1..];
            continue;
        };
        let name = &after[..close];
        match data.values.iter().find(|(key, _)| *key == name) {
            Some((_, value)) => out.push_str(value),
            None => return Err(name.to_string()),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Expands a format into styled segments: `{placeholder}` from the item's data, and
/// `[text](style)` with an inline style (ADR 0027). An unknown placeholder or a bad
/// inline style is an error; the caller warned at parse time and shows the format raw.
fn expand_spans(format: &str, data: &ItemData) -> Result<Vec<(String, Option<ItemStyle>)>, String> {
    let mut out: Vec<(String, Option<ItemStyle>)> = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < format.len() {
        let rest = &format[i..];
        if let Some(stripped) = rest.strip_prefix('[') {
            if let Some(close) = stripped.find("](") {
                let after = &stripped[close + 2..];
                if let Some(end) = after.find(')') {
                    let style = parse_item_style(&after[..end])?;
                    if !plain.is_empty() {
                        out.push((std::mem::take(&mut plain), None));
                    }
                    // The `[text]` may carry `{placeholder}`s too.
                    out.push((expand_placeholders(&stripped[..close], data)?, Some(style)));
                    i += 1 + close + 2 + end + 1;
                    continue;
                }
            }
            plain.push('[');
            i += 1;
        } else if let Some(stripped) = rest.strip_prefix('{') {
            if let Some(close) = stripped.find('}') {
                let name = &stripped[..close];
                match data.values.iter().find(|(key, _)| *key == name) {
                    Some((_, value)) => plain.push_str(value),
                    None => return Err(name.to_string()),
                }
                i += 1 + close + 1;
                continue;
            }
            plain.push('{');
            i += 1;
        } else {
            let c = rest.chars().next().expect("a non-empty rest");
            plain.push(c);
            i += c.len_utf8();
        }
    }
    if !plain.is_empty() {
        out.push((plain, None));
    }
    Ok(out)
}

/// The inline styles in a format's `[text](style)` segments, for parse-time validation.
fn inline_styles(format: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = format;
    while let Some(open) = rest.find("](") {
        let after = &rest[open + 2..];
        let Some(close) = after.find(')') else {
            break;
        };
        out.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    out
}

/// One `[tui.widgets]` slot as a line (ADR 0027 C3): the configured item ids in order, joined
/// by the footer's separator. `None` when none of them has data, so the row takes no space.
pub fn widget_line(app: &App, ids: &[String]) -> Option<Line<'static>> {
    let separator = " │ ";
    let separator_style = ItemStyle::Role(Role::FooterSeparator).resolve(app);
    let mut pieces: Vec<Piece> = Vec::new();
    for id in ids {
        let item = Item {
            id: id.clone(),
            icon: None,
            symbol_style: None,
            format: None,
            style: None,
            priority: components::priority(id),
            thresholds: components::thresholds(id),
            params: Vec::new(),
            disabled: false,
            unknown: components::builtin(id).is_none(),
        };
        if let Some(piece) = render_item(app, &item) {
            pieces.push(piece);
        }
    }
    if pieces.is_empty() {
        return None;
    }
    let mut spans: Vec<Span> = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(separator.to_string(), separator_style));
        }
        for (text, style) in &piece.spans {
            spans.push(Span::styled(text.clone(), *style));
        }
    }
    Some(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every non-ASCII glyph the `unicode` tier can draw (ADR 0027 and 6, plus
    /// ADR 0035's band half cells and the owner's 2026-10-16 glyph note: `×` for a
    /// failed tool head, `•` for a one-line tool head). Each one is checked against the six
    /// fonts by `scripts/check-glyph-coverage.py` and listed in the README's coverage table. A
    /// new glyph makes this test fail until it is re-checked and added here.
    const VERIFIED_COVERED: &str = "»│━─×▖▘▝▗↑↓✓•╭╰…▄▀";

    /// The documented exceptions to the coverage rule (ADR 0027/10 and ADR 0035's
    /// 2026-10-16 notes): the owner picked the arc and the pulse after seeing them, braille is
    /// an opt-in, and the fold marker pair `◼`/`◻` is the same kind of choice (four of the six
    /// reference fonts lack them; the owner chose them after seeing the set), so these may fall
    /// back to another font. Kept separate so the rest of the check is not loosened.
    const DOCUMENTED_EXCEPTIONS: &str = "◼◻◜◝◞◟⏺⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

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

    /// The brand mark is per glyph set (ADR 0027): Nerd Font, unicode `R`, ascii
    /// `rad`; `㎭` is gone.
    #[test]
    fn the_brand_mark_is_per_glyph_set() {
        for (name, expected) in [("unicode", "R"), ("nerd", "\u{f0c20}"), ("ascii", "rad")] {
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
    /// owner chose it knowing four of the six reference fonts lack it. The `ascii` tier and the
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
    /// (from the built-in presets), the spinner frames, the separators, the context bar and
    /// the auto marker are checked.
    #[test]
    fn unicode_tier_glyphs_are_all_verified_covered() {
        let mut glyphs: Vec<char> = Vec::new();
        for preset in [Preset::Deck, Preset::Full, Preset::Pi] {
            for row in preset_rows(preset, Glyphs::Unicode) {
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
                glyphs.extend(row.separator.chars());
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
            "context_bar",
            "tokens",
            "speed",
            "elapsed",
            "cost",
            "pending",
            "tasks",
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
    /// narrow-terminal truncation order.
    #[test]
    fn the_registry_matches_the_pre_refactor_priority_table() {
        // The `631c8dd` `item_priority` table.
        let expected: [(&str, i64); 20] = [
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
            ("context_bar", 85),
            ("tokens", 50),
            ("speed", 55),
            ("elapsed", 35),
            ("cost", 40),
            ("pending", 70),
            ("tasks", 72),
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
    /// `item_placeholders` table, id for id.
    #[test]
    fn the_registry_matches_the_pre_refactor_placeholder_table() {
        let expected: [(&str, &[&str]); 20] = [
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
            ("context_bar", &["bar", "percent", "used", "budget", "auto"]),
            ("tokens", &["in", "out", "cache", "cache_write"]),
            ("speed", &["speed"]),
            ("elapsed", &["elapsed"]),
            ("cost", &["usd"]),
            ("pending", &["count"]),
            ("tasks", &["running", "total"]),
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
    /// under `unicode` or a configured brand, ADR 0027/8).
    #[test]
    fn the_registry_matches_the_pre_refactor_role_table() {
        let app = app_with_glyphs("nerd");
        let expected: [(&str, Option<Role>); 20] = [
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
            ("context_bar", Some(Role::Budget)),
            ("tokens", Some(Role::Usage)),
            ("speed", Some(Role::Usage)),
            ("elapsed", Some(Role::Truncation)),
            ("cost", None),
            ("pending", Some(Role::TurnRunning)),
            ("tasks", Some(Role::TurnRunning)),
            ("mcp", Some(Role::Mcp)),
            ("scroll", None),
        ];
        for (id, role) in expected {
            let component = components::builtin(id).expect("registered");
            assert_eq!(component.role(&app), role, "default role for {id}");
        }
    }

    /// ADR 0027: the registry's default thresholds must match the `631c8dd`
    /// `default_thresholds` table (`context`/`context_bar`/`cost` at 70/90; every other id empty).
    #[test]
    fn the_registry_matches_the_pre_refactor_threshold_table() {
        let numeric = vec![
            (70, ItemStyle::Role(Role::BudgetOver)),
            (90, ItemStyle::Role(Role::ToolResultError)),
        ];
        for id in components::ids() {
            let expected = if matches!(id, "context" | "context_bar" | "cost") {
                numeric.clone()
            } else {
                Vec::new()
            };
            assert_eq!(components::thresholds(id), expected, "thresholds for {id}");
        }
    }

    /// ADR 0027: the registry's default glyph in the three selectable sets
    /// must match the `631c8dd` `glyph` table (`emoji`, index 3, is not selectable until slice ③
    /// and stays empty). The refactor gave `context_bar` the `context` icons; the old table had no
    /// `context_bar` row, so its default icon was empty in every set.
    #[test]
    fn the_registry_matches_the_pre_refactor_glyph_table() {
        let expected: [(&str, [&str; 3]); 20] = [
            ("brand", ["", "", ""]),
            ("host", ["@ ", "\u{f109} ", "host "]),
            ("model", ["", "", ""]),
            ("reasoning", ["* ", "\u{f0eb} ", "* "]),
            ("tier", ["", "", ""]),
            ("cwd", ["", "", ""]),
            ("git_branch", ["", "\u{e0a0} ", ""]),
            ("worktree", ["", "", ""]),
            ("status", ["", "", ""]),
            ("session", ["# ", "", "# "]),
            ("context", ["", "\u{f035b} ", "["]),
            ("context_bar", ["", "", ""]),
            ("tokens", ["", "", ""]),
            ("speed", ["\u{bb} ", "\u{f0e7} ", "spd "]),
            ("elapsed", ["", "\u{f017} ", "t "]),
            ("cost", ["", "", ""]),
            ("pending", ["", "", ""]),
            ("tasks", ["", "", ""]),
            ("mcp", ["", "", ""]),
            ("scroll", ["", "", ""]),
        ];
        assert_eq!(
            components::ids().len(),
            expected.len(),
            "registry/table size"
        );
        for (id, glyphs) in expected {
            let component = components::builtin(id).expect("registered");
            let [unicode, nerd, ascii, emoji] = component.glyphs();
            assert_eq!(
                [unicode, nerd, ascii],
                glyphs,
                "default glyphs for {id} (unicode/nerd/ascii)"
            );
            assert_eq!(emoji, "", "{id}'s emoji glyph is not selectable yet");
        }
    }

    /// ADR 0027: the draw path has no item-id tables. The pre-refactor
    /// dispatch (`item_placeholders`, `item_priority`, `default_thresholds`, the
    /// `CONTRIBUTION_ITEMS` list, …) is gone; the renderer reaches every item through
    /// `components::lookup`. The positive control pins that the lookup call is there, so this
    /// test cannot pass because dispatch moved nowhere.
    #[test]
    fn the_render_path_has_no_item_id_tables() {
        let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/footer.rs"))
            .expect("footer.rs is readable");
        // Only the non-test code: the test module names the removed functions on purpose.
        let code = source
            .split_once("\n#[cfg(test)]")
            .map_or(source.as_str(), |(before, _)| before);
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
            assert!(
                !code.contains(forbidden),
                "{forbidden} is back: the render path must not special-case item ids"
            );
        }
        assert!(
            code.contains("components::lookup("),
            "the renderer dispatches through the component registry"
        );
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
}
