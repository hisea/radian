//! The footer's resolved rows (moved from `footer.rs` by pure move, ADR 0043 decision 5):
//! item styles, rows and their positions, the row builder shared by user config and preset
//! files, and the legacy `[tui.widgets]` fold.

use radian_host::config::{
    TuiFooterConfig, TuiFooterItemConfig, TuiFooterRowConfig, TuiWidgetsConfig,
};
use ratatui::style::{Color, Modifier, Style};

use crate::app::App;
use crate::theme::Role;

use super::components;
use super::{inline_styles, role_style};

/// One item's style (ADR 0027): a theme role (follows the theme) or a fixed
/// starship-style color (does not). The theme stays the only color source for a role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ItemStyle {
    Role(Role),
    Fixed(Style),
}

impl ItemStyle {
    pub(crate) fn resolve(&self, app: &App) -> Style {
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
    /// The component's own parameters (`context`'s `width`/`fill`/`empty`, `status`'s
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
pub(crate) fn parse_item_style(text: &str) -> Result<ItemStyle, String> {
    if let Some(role) = parse_style(text) {
        return Ok(ItemStyle::Role(role));
    }
    parse_starship_style(text).map(ItemStyle::Fixed)
}

/// One configured row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) left: Vec<Item>,
    pub(crate) right: Vec<Item>,
    pub(crate) separator: String,
    /// The separator's style (ADR 0027): the `footer_separator` role by default,
    /// `[tui.footer] separator_style` can make it a color.
    pub(crate) separator_style: ItemStyle,
    /// Where the row draws (ADR 0027).
    pub(crate) position: RowPosition,
}

/// Where one footer row draws (ADR 0027): the status area, or a line
/// above/below the input box. The default is the footer.
///
/// The `position` values, in documentation order. The parser and the manual's drift test
/// both read this list.
pub const ROW_POSITIONS: &[&str] = &["footer", "above_input", "below_input"];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowPosition {
    /// The status area (the default).
    #[default]
    Footer,
    /// One line above the input box.
    AboveInput,
    /// One line below the input box.
    BelowInput,
}

impl RowPosition {
    fn parse(name: &str) -> Option<Self> {
        if !ROW_POSITIONS.contains(&name) {
            return None;
        }
        match name {
            "footer" => Some(Self::Footer),
            "above_input" => Some(Self::AboveInput),
            "below_input" => Some(Self::BelowInput),
            _ => None,
        }
    }

    /// The `position` spelling of this row position.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Footer => ROW_POSITIONS[0],
            Self::AboveInput => ROW_POSITIONS[1],
            Self::BelowInput => ROW_POSITIONS[2],
        }
    }
}

/// Folds the legacy `[tui.widgets]` block into footer rows (ADR 0027):
/// one row per non-empty slot, bare ids (a bare id takes `[tui.footer.items.<id>]` exactly like
/// a row id, so a colour configured there applies here too), positioned above/below the input
/// with the footer's separator. Returns the one deprecation warning, or `None` when the block
/// is empty. The block is removed after one version.
pub fn fold_legacy_widgets(
    config: &mut TuiFooterConfig,
    widgets: &TuiWidgetsConfig,
) -> Option<String> {
    if widgets.above_input.is_empty() && widgets.below_input.is_empty() {
        return None;
    }
    for (ids, position) in [
        (&widgets.above_input, "above_input"),
        (&widgets.below_input, "below_input"),
    ] {
        if ids.is_empty() {
            continue;
        }
        config.rows.push(TuiFooterRowConfig {
            left: ids
                .iter()
                .map(|id| TuiFooterItemConfig {
                    id: id.clone(),
                    ..Default::default()
                })
                .collect(),
            position: Some(position.to_string()),
            ..Default::default()
        });
    }
    Some(
        "tui.widgets is deprecated; move its ids into [[tui.footer.rows]] with position = \
         \"above_input\"|\"below_input\""
            .to_string(),
    )
}

impl Default for Row {
    fn default() -> Self {
        Self {
            left: Vec::new(),
            right: Vec::new(),
            separator: String::new(),
            separator_style: ItemStyle::Role(Role::FooterSeparator),
            position: RowPosition::Footer,
        }
    }
}

/// Builds `Row`s from parsed row configs: one shared path for user-configured rows and for
/// preset TOML rows (ADR 0027). A preset keeps its own separator
/// default (a user's `separator` never applied to the old Rust tables either); a bare id
/// takes the per-field `items` config, an inline table is used as written.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_rows(
    row_cfgs: &[TuiFooterRowConfig],
    default_separator: &str,
    separator_style: &ItemStyle,
    items: &std::collections::BTreeMap<String, TuiFooterItemConfig>,
    command_ids: &std::collections::BTreeSet<String>,
    aliased: &mut std::collections::BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> Vec<Row> {
    row_cfgs
        .iter()
        .map(|row| {
            let position = match row.position.as_deref() {
                None => RowPosition::Footer,
                Some(name) => RowPosition::parse(name).unwrap_or_else(|| {
                    warnings.push(format!(
                        "tui.footer.rows: position {name:?} is not one of {}; using {}",
                        ROW_POSITIONS.join("|"),
                        RowPosition::Footer.as_str()
                    ));
                    RowPosition::Footer
                }),
            };
            Row {
                left: row
                    .left
                    .iter()
                    .filter_map(|item| resolve_item(item, items, command_ids, aliased, warnings))
                    .collect(),
                right: row
                    .right
                    .iter()
                    .filter_map(|item| resolve_item(item, items, command_ids, aliased, warnings))
                    .collect(),
                separator: row
                    .separator
                    .clone()
                    .unwrap_or_else(|| default_separator.to_string()),
                separator_style: separator_style.clone(),
                position,
            }
        })
        .collect()
}

/// The current id a configured item id means. Two earlier-version ids keep working for one
/// version, each with one warning naming the new id:
/// - ADR 0027: `radian_host` knows which ids an earlier version used
///   for a provider's usage row; such an id stays an alias of `usage`.
/// - ADR 0027: `context_bar` merged into `context`, `{bar}` its
///   placeholder.
pub(crate) fn canonical_id(
    id: &str,
    warned: &mut std::collections::BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> String {
    // ADR 0027: the old `context_bar` id is the `context` component.
    if id == "context_bar" {
        if warned.insert(id.to_owned()) {
            warnings.push(
                "tui.footer: item \"context_bar\" is the old id for \"context\"; the old id \
                 works for one version"
                    .to_string(),
            );
        }
        return "context".to_owned();
    }
    let Some(current) = radian_host::usage_item_alias(id) else {
        return id.to_owned();
    };
    if warned.insert(id.to_owned()) {
        warnings.push(format!(
            "tui.footer: item {id:?} is the old id for {current:?}; the old id works for one \
             version"
        ));
    }
    current.to_owned()
}

/// Whether a parsed item carries no options of its own (a bare id in a row). A bare id takes
/// its options from `[tui.footer.items.<id>]`; an inline table does not (revision 3).
pub(crate) fn is_bare(config: &TuiFooterItemConfig) -> bool {
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
pub(crate) fn is_command_item(config: &TuiFooterItemConfig) -> bool {
    config.params.iter().any(|(key, _)| key == "command")
}

/// Resolves one configured item, recording its warnings. `None` when it is disabled.
fn resolve_item(
    config: &TuiFooterItemConfig,
    items: &std::collections::BTreeMap<String, TuiFooterItemConfig>,
    command_ids: &std::collections::BTreeSet<String>,
    warned: &mut std::collections::BTreeSet<String>,
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
    // An id an earlier version used for a provider's usage row is the `usage` component here
    // (ADR 0027), with one warning.
    let id = canonical_id(&effective.id, warned, warnings);
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
pub(crate) fn merge_into_item(
    item: &mut Item,
    config: &TuiFooterItemConfig,
    warnings: &mut Vec<String>,
) {
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
