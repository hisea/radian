//! The `[tui]` section (ADR 0029; footer ADR 0027 C2, widgets C3): the display
//! preferences the reader carries raw for the TUI.
//!
//! Split from `config/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change. A
//! display preference never refuses the file (the `tui.*` rule): unknown keys and wrong
//! types are skipped with a warning, except `tui.brand` wider than 4 display columns,
//! which is a config error (it would misalign the footer and the banner).

use std::path::Path;

use toml::Value;

#[cfg(test)]
use super::parse;
use super::{Config, ConfigError, wrong_type};

/// One `[tui.colors]` color value (ADR 0029 decision 5): a color literal (`#hex`,
/// `oklch(..)`, `okhsl(..)` or `""` for the terminal default) or a 256-color index.
/// The literal text is resolved by the TUI, which owns the color grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiColorValue {
    Text(String),
    Index(u8),
}

/// One `[tui.colors]` entry (ADR 0029 decision 5): the foreground/background colors and the
/// modifier forces. An absent field leaves the theme's value alone; `bold = false` etc.
/// removes the modifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiColorOverride {
    pub fg: Option<TuiColorValue>,
    pub bg: Option<TuiColorValue>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub dim: Option<bool>,
    pub underline: Option<bool>,
}

impl TuiColorOverride {
    /// Parses one color entry (ADR 0029 decisions 1 and 5): a bare string or a 0–255 index is
    /// the foreground; a table carries `fg`/`bg` and the `bold`/`italic`/`dim`/`underline`
    /// forces. `Err(reason)` names the first wrong field, so the caller can warn and skip.
    ///
    /// This is the **one** value-shape parser: `[tui.colors]` (ADR 0029 decision 5) and a theme
    /// file's `[roles]` (ADR 0029) both go through it, so the two cannot
    /// drift. The color grammar inside a value (`#hex`, `oklch()`, `okhsl()`, `""`, a variable)
    /// belongs to the TUI and is resolved after this shape is parsed.
    pub fn from_toml(value: &Value) -> Result<Self, String> {
        let mut parsed = Self::default();
        match value {
            Value::String(text) => {
                parsed.fg = Some(TuiColorValue::Text(text.clone()));
            }
            Value::Integer(index) if (0..=255).contains(index) => {
                parsed.fg = Some(TuiColorValue::Index(*index as u8));
            }
            Value::Table(spec) => {
                for (field, field_value) in spec {
                    match (field.as_str(), field_value) {
                        ("fg" | "bg", Value::String(text)) => {
                            let value = TuiColorValue::Text(text.clone());
                            if field == "fg" {
                                parsed.fg = Some(value);
                            } else {
                                parsed.bg = Some(value);
                            }
                        }
                        ("fg" | "bg", Value::Integer(index)) if (0..=255).contains(index) => {
                            let value = TuiColorValue::Index(*index as u8);
                            if field == "fg" {
                                parsed.fg = Some(value);
                            } else {
                                parsed.bg = Some(value);
                            }
                        }
                        ("bold", Value::Boolean(flag)) => parsed.bold = Some(*flag),
                        ("italic", Value::Boolean(flag)) => parsed.italic = Some(*flag),
                        ("dim", Value::Boolean(flag)) => parsed.dim = Some(*flag),
                        ("underline", Value::Boolean(flag)) => parsed.underline = Some(*flag),
                        _ => return Err(format!("`{field}` is not a color field")),
                    }
                }
            }
            _ => {
                return Err(
                    "not a color or a `{ fg, bg, bold, italic, dim, underline }` table".into(),
                );
            }
        }
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiConfig {
    /// `tui.theme`: a built-in name (`dark|light|mono`) or a theme file name (ADR 0029). The
    /// reader accepts any string — whether the name resolves is the TUI's finding, reported
    /// as a startup warning.
    pub theme: Option<String>,
    /// `tui.colors`: per-role color overrides, raw. The TUI validates the role names and
    /// colors; unknown or malformed entries are startup warnings (the `tui.*` rule).
    pub colors: Vec<(String, TuiColorOverride)>,
    /// The raw `tui.keybindings` table: action name → key strings. The TUI parses and
    /// validates it (its key grammar belongs to the terminal layer).
    pub keybindings: Vec<(String, Vec<String>)>,
    /// `tui.mouse`: whether the terminal reports the mouse wheel (ADR 0023 U2, plan B).
    /// `None` means the default, **on**; the TUI reads `unwrap_or(true)`. `false` sends no
    /// mouse sequence at all and leaves the terminal's native selection/copy alone.
    pub mouse: Option<bool>,
    /// `tui.input_style`: `"lines"` (the default: a rule above and below with a `❯ ` prompt) or
    /// `"box"` (the thin half frame, ADR 0035's 2026-10-16 note). Carried raw; the TUI parses it,
    /// and a value that is not `lines`/`box` — including the removed `"bar"` — warns once and
    /// falls back to `lines`, never a refusal (the `tui.*` display-field rule, ADR 0008).
    pub input_style: Option<String>,
    /// `tui.clipboard`: how a transcript-selection copy reaches the system clipboard —
    /// `"osc52"` (the default), `"pbcopy"` (macOS only; elsewhere a warning and OSC 52) or
    /// `"off"`. `None` means the default; an unknown value is ignored with a warning.
    pub clipboard: Option<String>,
    /// `tui.tips`: whether the startup screen shows one Tip line. `None`
    /// means the default, on.
    pub tips: Option<bool>,
    /// `tui.banner`: whether the startup screen shows the wordmark. `None`
    /// means the default, on.
    pub banner: Option<bool>,
    /// `tui.brand`: the brand text (ADR 0027). `None` means the glyph set's default
    /// (`nerd` `󰰠`, `unicode` `R`, `ascii` `rad`). Any string at most 4 display columns wide;
    /// wider is a config error (it would break the footer and the banner).
    pub brand: Option<String>,
    /// `tui.spinner`: `"pulse"` (the default), `"arc"`, `"quadrant"` or `"braille"`
    /// (ADR 0027). The TUI validates it; an unknown value warns and uses `pulse`.
    pub spinner: Option<String>,
    /// `tui.usage_period_secs`: the opencode-go usage refresh period (ADR 0027 C4). `None` means
    /// the default (60 s); `0` disables the contributor; other values are clamped to 15–600 s.
    pub usage_period_secs: Option<u64>,
    /// `tui.display.reasoning`: `"collapsed"` (the default), `"expanded"` or `"hidden"`
    /// (ADR 0028 R3). `None` means the default; an unknown value is ignored with a warning.
    pub display_reasoning: Option<String>,
    /// `tui.display.tool_output`: `"collapsed"`, `"expanded"` or `"errors"` (the default,
    /// ADR 0028 R4). `None` means the default; an unknown value is ignored with a warning.
    pub display_tool_output: Option<String>,
    /// `tui.display.tool_preview_lines`: how many body lines a collapsed tool card shows
    /// (default 5). `None` means the default; a non-positive or non-integer value is skipped
    /// with a warning.
    pub display_tool_preview_lines: Option<u64>,
    /// `tui.display.mermaid` (ADR 0028 R8): draw a closed ```` ```mermaid ```` block as a
    /// Unicode diagram. `None` means the default (`true`); an unknown value is ignored with a
    /// warning.
    pub display_mermaid: Option<bool>,
    /// `tui.display.highlight` (ADR 0028 R2): syntax-highlight a closed code block whose info
    /// string names a common language. `None` means the default (`true`); an unknown value is
    /// ignored with a warning.
    pub display_highlight: Option<bool>,
    /// `[tui.footer]` (ADR 0027 C2): the footer's rows and their items. Carried in a shape the
    /// TUI can consume; the reader validates types and the known top-level keys only. An
    /// unknown item id, placeholder or parameter is the TUI's warning, never a refusal.
    pub footer: TuiFooterConfig,
    /// `[tui.widgets]` (ADR 0027 C3): contributor items shown on a line above or below the
    /// input box, as item ids.
    pub widgets: TuiWidgetsConfig,
}

/// `[tui.widgets]` (ADR 0027 C3): the item ids shown `above_input` and `below_input`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiWidgetsConfig {
    pub above_input: Vec<String>,
    pub below_input: Vec<String>,
}

/// `[tui.footer]` (ADR 0027 C2). Each row has a `left` and a `right` group of items; an item
/// is a bare id string or a table with `item` plus options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterConfig {
    /// `preset`: `"full"` (the default, ADR 0027), `"pi"` or `"classic"`. Any
    /// other value is the TUI's warning.
    pub preset: Option<String>,
    /// `separator`: the glyph(s) between items; a row may override it.
    pub separator: Option<String>,
    /// `separator_style`: the separator's style — a theme role or a color (ADR 0027 revision
    /// 3). The TUI parses it; the default is the `footer_separator` role.
    pub separator_style: Option<String>,
    /// `glyphs`: `"unicode"` (the default), `"nerd"`, `"ascii"` or `"emoji"` (ADR 0027
    /// revision 14 decision 7). The TUI validates it.
    pub glyphs: Option<String>,
    /// `context_line`: `"off"` (the default) or `"percentage"`. ADR 0027 item 4:
    /// `percentage` fills the gap between a row's left and right groups with the context
    /// usage (the used fraction in the budget role, the rest in the border role).
    pub context_line: Option<String>,
    /// `transparent`: `false` (the default) or `true`. ADR 0027 item 5 (omp's
    /// `statusLine.transparent`): the footer inherits the terminal's background - every
    /// footer cell's background is cleared, a configured item style's included.
    pub transparent: Option<bool>,
    /// `[tui.footer.items.<id>]`: per-field defaults for one item, applied when a row names the
    /// item by bare id (ADR 0027). A row's inline table wins over this.
    pub items: std::collections::BTreeMap<String, TuiFooterItemConfig>,
    pub rows: Vec<TuiFooterRowConfig>,
}

/// One `[[tui.footer.rows]]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterRowConfig {
    pub left: Vec<TuiFooterItemConfig>,
    pub right: Vec<TuiFooterItemConfig>,
    /// The row's own `separator`, overriding the footer's.
    pub separator: Option<String>,
    /// Where the row draws (ADR 0027): `"footer"` (the default),
    /// `"above_input"` or `"below_input"`. Carried raw; the TUI validates it.
    pub position: Option<String>,
}

/// One footer item: either a bare id or a table with `item` plus options. Options the reader
/// does not know are kept as text in `params`, so the TUI can warn about (and show) them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiFooterItemConfig {
    pub id: String,
    pub icon: Option<String>,
    /// `symbol_style`: the icon's own style, when it differs from the text's.
    pub symbol_style: Option<String>,
    /// `disabled = true` hides the item everywhere it is named (ADR 0027).
    pub disabled: bool,
    pub format: Option<String>,
    pub style: Option<String>,
    /// The item's priority for narrow-terminal truncation (higher survives longer).
    pub priority: Option<i64>,
    /// `thresholds = [{ at = 70, style = "warn" }, …]` in file order.
    pub thresholds: Vec<(i64, String)>,
    /// The item's own parameters (`width`, `fill`, `empty`, …) as text.
    pub params: Vec<(String, String)>,
}

pub(super) fn parse_tui(
    value: &Value,
    path: &Path,
    config: &mut Config,
) -> Result<(), ConfigError> {
    let Some(table) = value.as_table() else {
        return Err(wrong_type(path, "tui", "a table"));
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            // Any name is accepted (ADR 0029 decision 1): built-in names resolve directly,
            // anything else is looked up in `~/.radian/themes/<name>.toml` by the TUI, which
            // warns and falls back when neither exists. A display preference never refuses
            // the file.
            ("theme", Value::String(theme)) => config.tui.theme = Some(theme.clone()),
            ("theme", _) => config.skipped.push("tui.theme".into()),
            ("colors", Value::Table(colors)) => {
                for (key, value) in colors {
                    // The one value-shape parser (ADR 0029 decisions 1/5); a malformed entry is
                    // a display preference: skipped by name, never a refusal.
                    match TuiColorOverride::from_toml(value) {
                        Ok(parsed) => config.tui.colors.push((key.clone(), parsed)),
                        Err(_) => config.skipped.push(format!("tui.colors.{key}")),
                    }
                }
            }
            ("colors", _) => return Err(wrong_type(path, "tui.colors", "a table")),
            ("keybindings", Value::Table(bindings)) => {
                for (action, keys) in bindings {
                    match keys.as_array() {
                        Some(keys) => {
                            let keys: Vec<String> = keys
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect();
                            config.tui.keybindings.push((action.clone(), keys));
                        }
                        None => config.skipped.push(format!("tui.keybindings.{action}")),
                    }
                }
            }
            ("keybindings", _) => return Err(wrong_type(path, "tui.keybindings", "a table")),
            ("mouse", Value::Boolean(mouse)) => config.tui.mouse = Some(*mouse),
            // The input style is a display preference carried raw (like `tui.spinner`): the TUI
            // parses it, so an unknown value (the removed `bar` included) warns there, with the
            // exact spelling, and falls back to `lines` -- never a refusal (ADR 0008).
            ("input_style", Value::String(style)) => config.tui.input_style = Some(style.clone()),
            ("input_style", _) => config.skipped.push("tui.input_style".into()),
            ("footer", Value::Table(footer)) => {
                if let Err(key) = parse_footer(footer, config) {
                    config.skipped.push(key);
                }
            }
            ("footer", _) => config.skipped.push("tui.footer".into()),
            ("widgets", Value::Table(widgets)) => {
                if let Err(key) = parse_widgets(widgets, config) {
                    config.skipped.push(key);
                }
            }
            ("widgets", _) => config.skipped.push("tui.widgets".into()),
            ("usage_period_secs", Value::Integer(seconds)) => {
                config.tui.usage_period_secs = Some(*seconds as u64);
            }
            ("display", Value::Table(display)) => {
                for (key, value) in display {
                    match (key.as_str(), value) {
                        ("reasoning", Value::String(mode))
                            if matches!(mode.as_str(), "collapsed" | "expanded" | "hidden") =>
                        {
                            config.tui.display_reasoning = Some(mode.clone());
                        }
                        ("tool_output", Value::String(mode))
                            if matches!(mode.as_str(), "collapsed" | "expanded" | "errors") =>
                        {
                            config.tui.display_tool_output = Some(mode.clone());
                        }
                        ("tool_preview_lines", Value::Integer(lines)) if *lines > 0 => {
                            config.tui.display_tool_preview_lines = Some(*lines as u64);
                        }
                        ("mermaid", Value::Boolean(on)) => {
                            config.tui.display_mermaid = Some(*on);
                        }
                        ("highlight", Value::Boolean(on)) => {
                            config.tui.display_highlight = Some(*on);
                        }
                        // A display preference: skip the key with a warning, never refuse the
                        // file. The TUI keeps its default.
                        _ => config.skipped.push(format!("tui.display.{key}")),
                    }
                }
            }
            ("display", _) => config.skipped.push("tui.display".into()),
            ("clipboard", Value::String(mode))
                if matches!(mode.as_str(), "osc52" | "pbcopy" | "off") =>
            {
                config.tui.clipboard = Some(mode.clone());
            }
            // An unknown clipboard transport is a display preference too: skip it with the
            // same warning, and the TUI keeps its default.
            ("clipboard", _) => config.skipped.push("tui.clipboard".into()),
            // A non-boolean mouse is a display preference: ignore it (and say so), never
            // refuse the file.
            ("mouse", _) => config.skipped.push("tui.mouse".into()),
            ("tips", Value::Boolean(tips)) => config.tui.tips = Some(*tips),
            // A non-boolean tips flag is the same kind of display preference.
            ("tips", _) => config.skipped.push("tui.tips".into()),
            ("banner", Value::Boolean(banner)) => config.tui.banner = Some(*banner),
            ("banner", _) => config.skipped.push("tui.banner".into()),
            // ADR 0027: the brand is any short text (≤ 4 display columns). Wider is
            // a config error, not a skipped key: it would misalign the footer and the banner.
            ("brand", Value::String(brand)) => {
                if unicode_width::UnicodeWidthStr::width(brand.as_str()) > 4 {
                    return Err(wrong_type(
                        path,
                        "tui.brand",
                        "a string at most 4 display columns wide",
                    ));
                }
                config.tui.brand = Some(brand.clone());
            }
            // A non-string brand is a display preference: skip it with a warning.
            ("brand", _) => config.skipped.push("tui.brand".into()),
            // ADR 0027: the spinner kind is a display preference; the TUI
            // validates the name and falls back with a warning.
            ("spinner", Value::String(spinner)) => config.tui.spinner = Some(spinner.clone()),
            ("spinner", _) => config.skipped.push("tui.spinner".into()),
            (other, _) => config.skipped.push(format!("tui.{other}")),
        }
    }
    Ok(())
}

/// Parses `[tui.widgets]` (ADR 0027 C3). A non-string entry is skipped with a warning.
fn parse_widgets(table: &toml::map::Map<String, Value>, config: &mut Config) -> Result<(), String> {
    let mut widgets = TuiWidgetsConfig::default();
    for (key, value) in table {
        let slot = match key.as_str() {
            "above_input" => &mut widgets.above_input,
            "below_input" => &mut widgets.below_input,
            _ => return Err(format!("tui.widgets.{key}")),
        };
        let Value::Array(entries) = value else {
            return Err(format!("tui.widgets.{key}"));
        };
        for entry in entries {
            match entry {
                Value::String(id) => slot.push(id.clone()),
                _ => return Err(format!("tui.widgets.{key}")),
            }
        }
    }
    config.tui.widgets = widgets;
    Ok(())
}

/// Parses `[tui.footer]` (ADR 0027 C2). A structural problem names the key it skipped; a
/// display preference never refuses the file (the `tui.*` rule).
fn parse_footer(table: &toml::map::Map<String, Value>, config: &mut Config) -> Result<(), String> {
    let mut footer = TuiFooterConfig::default();
    for (key, value) in table {
        match (key.as_str(), value) {
            ("preset", Value::String(preset)) => footer.preset = Some(preset.clone()),
            ("separator", Value::String(separator)) => footer.separator = Some(separator.clone()),
            ("separator_style", Value::String(style)) => {
                footer.separator_style = Some(style.clone())
            }
            ("glyphs", Value::String(glyphs)) => footer.glyphs = Some(glyphs.clone()),
            // ADR 0027 item 4: the left/right gap fill.
            ("context_line", Value::String(mode)) => footer.context_line = Some(mode.clone()),
            ("transparent", Value::Boolean(on)) => footer.transparent = Some(*on),
            ("items", Value::Table(items)) => {
                for (id, value) in items {
                    let Value::Table(table) = value else {
                        return Err(format!("tui.footer.items.{id}"));
                    };
                    let item = parse_footer_item_table(table, id)?;
                    footer.items.insert(id.clone(), item);
                }
            }
            ("rows", Value::Array(rows)) => {
                for row in rows {
                    let Some(row) = row.as_table() else {
                        return Err("tui.footer.rows".into());
                    };
                    footer.rows.push(parse_footer_row(row)?);
                }
            }
            (other, _) => return Err(format!("tui.footer.{other}")),
        }
    }
    config.tui.footer = footer;
    Ok(())
}

fn parse_footer_row(table: &toml::map::Map<String, Value>) -> Result<TuiFooterRowConfig, String> {
    let mut row = TuiFooterRowConfig::default();
    for (key, value) in table {
        match (key.as_str(), value) {
            ("left", Value::Array(items)) => {
                for item in items {
                    row.left.push(parse_footer_item(item)?);
                }
            }
            ("right", Value::Array(items)) => {
                for item in items {
                    row.right.push(parse_footer_item(item)?);
                }
            }
            ("separator", Value::String(separator)) => row.separator = Some(separator.clone()),
            // ADR 0027: the row's position (footer/above/below_input).
            ("position", Value::String(position)) => row.position = Some(position.clone()),
            (other, _) => return Err(format!("tui.footer.rows.{other}")),
        }
    }
    Ok(row)
}

/// One item: a bare id string, or a table with `item` plus options. Anything else is skipped.
fn parse_footer_item(value: &Value) -> Result<TuiFooterItemConfig, String> {
    match value {
        Value::String(id) => Ok(TuiFooterItemConfig {
            id: id.clone(),
            ..Default::default()
        }),
        Value::Table(table) => {
            let id = table
                .get("item")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if id.is_empty() {
                return Err("tui.footer.rows.item".into());
            }
            parse_footer_item_table(table, &id)
        }
        _ => Err("tui.footer.rows.item".into()),
    }
}

/// The options of one item table. `id` is known already: from the `item` key in a row, or from
/// the key of a `[tui.footer.items.<id>]` table. `icon` and `symbol` are the same field
/// (ADR 0027 C2 used `icon`, revision 3 calls it `symbol`); a known option with the wrong type
/// is skipped, not a refusal.
fn parse_footer_item_table(
    table: &toml::map::Map<String, Value>,
    id: &str,
) -> Result<TuiFooterItemConfig, String> {
    let mut item = TuiFooterItemConfig {
        id: id.to_owned(),
        ..Default::default()
    };
    for (key, value) in table {
        match (key.as_str(), value) {
            ("item", Value::String(_)) => {}
            ("icon" | "symbol", Value::String(symbol)) => item.icon = Some(symbol.clone()),
            ("symbol_style", Value::String(style)) => item.symbol_style = Some(style.clone()),
            ("format", Value::String(format)) => item.format = Some(format.clone()),
            ("style", Value::String(style)) => item.style = Some(style.clone()),
            ("disabled", Value::Boolean(disabled)) => item.disabled = *disabled,
            ("priority", Value::Integer(priority)) => item.priority = Some(*priority),
            ("thresholds", Value::Array(thresholds)) => {
                for threshold in thresholds {
                    let Some(threshold) = threshold.as_table() else {
                        return Err("tui.footer.rows.thresholds".into());
                    };
                    let at = threshold.get("at").and_then(Value::as_integer);
                    let style = threshold.get("style").and_then(Value::as_str);
                    match (at, style) {
                        (Some(at), Some(style)) => {
                            item.thresholds.push((at, style.to_owned()));
                        }
                        _ => return Err("tui.footer.rows.thresholds".into()),
                    }
                }
            }
            (
                "item" | "icon" | "symbol" | "symbol_style" | "format" | "style" | "disabled"
                | "priority",
                _,
            ) => {
                return Err("tui.footer.rows.item".into());
            }
            // Everything else is the item's own parameter, kept as text.
            (param, value) => {
                if let Some(text) = value_to_text(value) {
                    item.params.push((param.to_owned(), text));
                } else {
                    return Err(format!("tui.footer.rows.{param}"));
                }
            }
        }
    }
    Ok(item)
}

/// A scalar parameter as the text the TUI parses (`20`, `"─"`, `true`).
fn value_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Integer(number) => Some(number.to_string()),
        Value::Float(number) => Some(number.to_string()),
        Value::Boolean(flag) => Some(flag.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod startup_pref_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0008: `tui.tips`/`tui.banner` are display booleans; a non-boolean is skipped with a
    /// warning, never a refusal (the `tui.*` rule).
    #[test]
    fn tui_tips_and_banner_are_carried_and_non_booleans_are_skipped() {
        let config =
            parse_str("schema_version = 1\n[tui]\ntips = false\nbanner = false\n").unwrap();
        assert_eq!(config.tui.tips, Some(false));
        assert_eq!(config.tui.banner, Some(false));

        let config = parse_str("schema_version = 1\n[tui]\ntips = \"yes\"\n").unwrap();
        assert_eq!(config.tui.tips, None);
        assert!(config.skipped.contains(&"tui.tips".to_string()));
    }

    /// ADR 0027: `tui.brand` is carried, a non-string is skipped (the `tui.*`
    /// rule), and a value wider than 4 display columns is a config error (it would misalign
    /// the footer and the banner).
    #[test]
    fn tui_brand_is_carried_and_over_wide_is_refused() {
        let config = parse_str("schema_version = 1\n[tui]\nbrand = \"ʀ\"\n").unwrap();
        assert_eq!(config.tui.brand.as_deref(), Some("ʀ"));

        // A non-string is a display preference: skipped, never a refusal.
        let config = parse_str("schema_version = 1\n[tui]\nbrand = 3\n").unwrap();
        assert_eq!(config.tui.brand, None);
        assert!(config.skipped.contains(&"tui.brand".to_string()));

        // `㎭` is East-Asian-Wide (2 columns): two are 4 (fine), three are 5 (refused).
        assert!(parse_str("schema_version = 1\n[tui]\nbrand = \"㎭㎭\"\n").is_ok());
        let error = parse_str("schema_version = 1\n[tui]\nbrand = \"㎭㎭R\"\n").unwrap_err();
        assert!(matches!(error, ConfigError::WrongType { .. }), "{error}");
    }

    /// ADR 0027: `tui.spinner` is carried; a non-string is skipped (the `tui.*`
    /// rule). The TUI validates the name and falls back to `pulse`.
    #[test]
    fn tui_spinner_is_carried_and_non_strings_are_skipped() {
        let config = parse_str("schema_version = 1\n[tui]\nspinner = \"quadrant\"\n").unwrap();
        assert_eq!(config.tui.spinner.as_deref(), Some("quadrant"));

        let config = parse_str("schema_version = 1\n[tui]\nspinner = 3\n").unwrap();
        assert_eq!(config.tui.spinner, None);
        assert!(config.skipped.contains(&"tui.spinner".to_string()));
    }
}

#[cfg(test)]
mod footer_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0027 C2: the full `[tui.footer]` shape — preset, separator, glyphs, two rows, both
    /// item spellings, an option table and a threshold.
    #[test]
    fn the_footer_config_is_carried() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.footer]\n",
            "preset = \"pi\"\n",
            "separator = \" | \"\n",
            "glyphs = \"unicode\"\n",
            "context_line = \"percentage\"\n",
            "transparent = true\n",
            "[[tui.footer.rows]]\n",
            "left = [\n",
            "  { item = \"model\", format = \"{display_name}\" },\n",
            "  { item = \"context_bar\", width = 20, fill = \"━\", empty = \"─\", format = \"{bar} {percent}%\" },\n",
            "]\n",
            "right = [\"reasoning\"]\n",
            "[[tui.footer.rows]]\n",
            "left = [\"tokens\", \"cost\"]\n",
            "separator = \" · \"\n",
            "[[tui.footer.rows]]\n",
            "left = [{ item = \"context\", thresholds = [{ at = 70, style = \"warn\" }, { at = 90, style = \"error\" }] }]\n",
        ))
        .unwrap();
        let footer = &config.tui.footer;
        assert_eq!(footer.preset.as_deref(), Some("pi"));
        assert_eq!(footer.separator.as_deref(), Some(" | "));
        assert_eq!(footer.glyphs.as_deref(), Some("unicode"));
        assert_eq!(footer.context_line.as_deref(), Some("percentage"));
        assert_eq!(footer.transparent, Some(true));
        assert_eq!(footer.rows.len(), 3);
        assert_eq!(footer.rows[0].left.len(), 2);
        assert_eq!(footer.rows[0].left[0].id, "model");
        assert_eq!(
            footer.rows[0].left[0].format.as_deref(),
            Some("{display_name}")
        );
        assert_eq!(footer.rows[0].left[1].params.len(), 3);
        assert_eq!(footer.rows[0].right[0].id, "reasoning");
        assert_eq!(footer.rows[1].separator.as_deref(), Some(" · "));
        assert_eq!(footer.rows[1].left[1].id, "cost");
        assert_eq!(
            footer.rows[2].left[0].thresholds,
            vec![(70, "warn".into()), (90, "error".into())]
        );
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }

    /// ADR 0027: a row carries its `position`; an unknown position
    /// value is carried raw (the TUI warns and falls back to the footer).
    #[test]
    fn a_row_carries_its_position() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[tui.footer.rows]]\n",
            "left = [\"status\"]\n",
            "position = \"above_input\"\n",
        ))
        .unwrap();
        assert_eq!(
            config.tui.footer.rows[0].position.as_deref(),
            Some("above_input")
        );
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }

    /// An unknown item id is **carried** (the TUI warns and shows it raw), but an unknown
    /// `[tui.footer]` key or a wrong type is skipped with a warning, never refused.
    #[test]
    fn unknown_footer_keys_and_types_are_skipped_never_refused() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.footer]\n",
            "future_key = true\n",
            "[[tui.footer.rows]]\n",
            "left = [{ item = \"future_item\" }, 5, \"model\"]\n",
        ))
        .unwrap();
        // `future_key` skipped; the row is dropped whole (its second item is not a string or a
        // table), so the footer falls back to the default.
        assert!(
            config
                .skipped
                .iter()
                .any(|key| key.starts_with("tui.footer")),
            "{:?}",
            config.skipped
        );
        assert!(config.tui.footer.rows.is_empty());

        // A well-formed row with an unknown item id keeps the id.
        let config = parse_str(
            "schema_version = 1\n[tui.footer]\n[[tui.footer.rows]]\nleft = [\"future_item\"]\n",
        )
        .unwrap();
        assert_eq!(config.tui.footer.rows[0].left[0].id, "future_item");
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }
}

#[cfg(test)]
mod widget_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Result<Config, ConfigError> {
        parse(Path::new("/tmp/test/config.toml"), text)
    }

    /// ADR 0027 C3: `[tui.widgets]` carries the item ids above/below the input box.
    #[test]
    fn tui_widgets_carry_the_item_ids() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[tui.widgets]\n",
            "above_input = [\"status\"]\n",
            "below_input = [\"usage\"]\n",
        ))
        .unwrap();
        assert_eq!(config.tui.widgets.above_input, vec!["status".to_string()]);
        assert_eq!(config.tui.widgets.below_input, vec!["usage".to_string()]);
        // An unknown key or a non-string entry is skipped, never refused.
        let config = parse_str("schema_version = 1\n[tui.widgets]\nfuture = [\"x\"]\n").unwrap();
        assert!(config.skipped.iter().any(|k| k == "tui.widgets.future"));
    }
}
