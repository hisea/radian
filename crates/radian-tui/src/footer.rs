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
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::theme::Role;

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
}

/// Which footer the session shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// pi's two rows: model/cwd/branch/context bar/reasoning, then tokens/cost.
    #[default]
    Pi,
    /// Today's one status line, byte-for-byte (drawn by the renderer).
    Classic,
}

impl Preset {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "pi" => Some(Self::Pi),
            "classic" => Some(Self::Classic),
            _ => None,
        }
    }
}

/// One resolved item.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    /// The configured id as written (an unknown id is shown raw).
    id: String,
    /// The item's own `icon`, else the built-in glyph for the id.
    icon: Option<String>,
    format: Option<String>,
    style: Option<Role>,
    priority: i64,
    thresholds: Vec<(i64, Role)>,
    params: Vec<(String, String)>,
    /// `false` for an unknown id: the item shows its id text.
    known: bool,
}

/// One configured row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Row {
    left: Vec<Item>,
    right: Vec<Item>,
    separator: String,
}

/// The session's footer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Footer {
    preset: Preset,
    glyphs: Glyphs,
    rows: Vec<Row>,
}

impl Footer {
    /// Parses the configuration. Unknown item ids, placeholders and parameters are shown raw;
    /// their warnings are collected here (once) and shown at startup.
    pub fn from_config(config: &TuiFooterConfig) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let preset = match config.preset.as_deref() {
            None => Preset::Pi,
            Some(name) => Preset::parse(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "tui.footer.preset = {name:?} is not one of pi|classic; using pi"
                ));
                Preset::Pi
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
        let mut rows = Vec::new();
        for row in &config.rows {
            rows.push(Row {
                left: row
                    .left
                    .iter()
                    .map(|item| resolve_item(item, &mut warnings))
                    .collect(),
                right: row
                    .right
                    .iter()
                    .map(|item| resolve_item(item, &mut warnings))
                    .collect(),
                separator: row.separator.clone().unwrap_or_else(|| separator.clone()),
            });
        }
        (
            Self {
                preset,
                glyphs,
                rows,
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
            Cow::Owned(default_rows())
        } else {
            Cow::Borrowed(&self.rows)
        };
        rows.iter().map(|row| render_row(app, row, width)).collect()
    }
}

/// The default two-row `pi` layout (ADR 0027 C2 / ).
fn default_rows() -> Vec<Row> {
    let item = |id: &str| Item {
        id: id.to_string(),
        icon: None,
        format: None,
        style: None,
        priority: item_priority(id),
        thresholds: default_thresholds(id),
        params: Vec::new(),
        known: true,
    };
    vec![
        Row {
            left: vec![
                item("model"),
                item("cwd"),
                item("git_branch"),
                item("context_bar"),
            ],
            right: vec![item("reasoning")],
            separator: " │ ".to_string(),
        },
        Row {
            left: vec![item("tokens"), item("cost")],
            right: Vec::new(),
            separator: " │ ".to_string(),
        },
    ]
}

/// Resolves one configured item, recording its warnings.
fn resolve_item(config: &TuiFooterItemConfig, warnings: &mut Vec<String>) -> Item {
    let known = item_is_known(&config.id);
    if !known {
        warnings.push(format!(
            "tui.footer: unknown item {:?}; showing it as written",
            config.id
        ));
    }
    let style = config.style.as_deref().and_then(|name| {
        parse_style(name).or_else(|| {
            warnings.push(format!(
                "tui.footer: item {:?} has unknown style {name:?}; using its default",
                config.id
            ));
            None
        })
    });
    let mut thresholds = Vec::new();
    if config.thresholds.is_empty() {
        thresholds = default_thresholds(&config.id);
    } else {
        for (at, name) in &config.thresholds {
            match parse_style(name) {
                Some(role) => thresholds.push((*at, role)),
                None => warnings.push(format!(
                    "tui.footer: item {:?} has unknown threshold style {name:?}; ignoring it",
                    config.id
                )),
            }
        }
    }
    // Validate the item's placeholders once, so the draw path never warns.
    if let Some(format) = &config.format {
        for placeholder in placeholders(format) {
            if !item_placeholders(&config.id).contains(&placeholder.as_str()) {
                warnings.push(format!(
                    "tui.footer: item {:?} has unknown placeholder {{{placeholder}}}; showing \
                     the format as written",
                    config.id
                ));
            }
        }
    }
    Item {
        id: config.id.clone(),
        icon: config.icon.clone(),
        format: config.format.clone(),
        style,
        priority: config.priority.unwrap_or_else(|| item_priority(&config.id)),
        thresholds,
        params: config.params.clone(),
        known,
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

/// The built-in item ids (ADR 0027 C2; `worktree`/`status` are slice 2).
fn item_is_known(id: &str) -> bool {
    !item_placeholders(id).is_empty()
}

/// The placeholders an item defines; empty for an unknown id.
fn item_placeholders(id: &str) -> &'static [&'static str] {
    match id {
        "model" => &["display_name", "id", "provider"],
        "reasoning" => &["level"],
        "tier" => &["tier"],
        "cwd" => &["path", "path_home", "basename"],
        "git_branch" => &["branch", "dirty"],
        "session" => &["id"],
        "context" => &["bar", "percent", "used", "budget"],
        "context_bar" => &["bar", "percent", "used", "budget"],
        "tokens" => &["in", "out", "cache"],
        "cost" => &["usd"],
        "pending" => &["count"],
        "mcp" => &["mcp"],
        "scroll" => &["position", "mode"],
        _ => &[],
    }
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

/// The default truncation priority: higher survives a narrow terminal longer.
fn item_priority(id: &str) -> i64 {
    match id {
        "model" => 90,
        "context" | "context_bar" => 85,
        "cwd" => 80,
        "tier" => 75,
        "git_branch" | "pending" => 70,
        "session" => 65,
        "reasoning" => 60,
        "mcp" => 55,
        "tokens" => 50,
        "cost" => 40,
        "scroll" => 20,
        _ => 50,
    }
}

/// An item's default theme role (the classic status bar's assignments, kept so a default
/// footer reads the same way). `None` means the terminal's default style.
fn default_role(id: &str) -> Option<Role> {
    Some(match id {
        "model" => Role::Model,
        "tier" => Role::Tier,
        "tokens" => Role::Usage,
        "mcp" => Role::Mcp,
        "context" | "context_bar" => Role::Budget,
        "pending" => Role::TurnRunning,
        _ => return None,
    })
}

/// The role for the `reasoning` item at `level` (ADR 0027 C2, one role per level).
fn reasoning_role(level: &str) -> Role {
    match level {
        "off" => Role::ReasoningOff,
        "minimal" => Role::ReasoningMinimal,
        "low" => Role::ReasoningLow,
        "medium" => Role::ReasoningMedium,
        "high" => Role::ReasoningHigh,
        "xhigh" => Role::ReasoningXhigh,
        "max" => Role::ReasoningMax,
        _ => Role::ReasoningMedium,
    }
}

/// The default 70/90 thresholds for the numeric items (ADR 0027 C2).
fn default_thresholds(id: &str) -> Vec<(i64, Role)> {
    match id {
        "context" | "context_bar" | "cost" => {
            vec![(70, Role::BudgetOver), (90, Role::ToolResultError)]
        }
        _ => Vec::new(),
    }
}

/// One styled piece of a row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Piece {
    text: String,
    style: Style,
    priority: i64,
}

fn render_row(app: &App, row: &Row, width: usize) -> Line<'static> {
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

    let left_text = join_group(&left, separator);
    let right_text = join_group(&right, separator);
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
            spans.push(Span::raw(" ".repeat(gap)));
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
    let text: usize = pieces.iter().map(|piece| width_of(&piece.text)).sum();
    let separators = width_of(separator) * pieces.len().saturating_sub(1);
    text + separators
}

/// One group as (text, style) pairs, its items joined by `separator`.
fn join_group(pieces: &[Piece], separator: &str) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            out.push((separator.to_string(), Style::default()));
        }
        out.push((piece.text.clone(), piece.style));
    }
    out
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// One item's text and style, or `None` when it has no data (never a fabricated number).
fn render_item(app: &App, item: &Item) -> Option<Piece> {
    if !item.known {
        return Some(Piece {
            text: item.id.clone(),
            style: item
                .style
                .map(|role| role_style(app, role))
                .unwrap_or_default(),
            priority: item.priority,
        });
    }
    let data = item_data(app, &item.id, item)?;
    let format = item
        .format
        .clone()
        .unwrap_or_else(|| data.default_format.to_string());
    let text = match expand(&format, &data) {
        Ok(text) => text,
        // An unknown placeholder was already warned about at parse time: show it raw.
        Err(_) => format,
    };
    let icon = item
        .icon
        .clone()
        .unwrap_or_else(|| glyph(&item.id, app.footer_glyphs()).to_string());
    let text = if icon.is_empty() {
        text
    } else {
        format!("{icon}{text}")
    };
    let role = if item.id == "reasoning" {
        Some(
            app.next_thinking
                .as_deref()
                .or(app.thinking_level.as_deref())
                .map(reasoning_role)
                .unwrap_or(Role::ReasoningMedium),
        )
    } else {
        item.thresholds
            .iter()
            .rev()
            .find(|(at, _)| data.numeric.is_some_and(|value| value as i64 >= *at))
            .map(|(_, role)| *role)
            .or(item.style)
            .or_else(|| default_role(&item.id))
    };
    Some(Piece {
        text,
        style: role.map(|role| role_style(app, role)).unwrap_or_default(),
        priority: item.priority,
    })
}

fn role_style(app: &App, role: Role) -> Style {
    let mut theme = app.theme.clone();
    *role.style_mut(&mut theme)
}

/// The facts one item reads.
struct ItemData<'a> {
    values: Vec<(&'a str, String)>,
    default_format: &'a str,
    /// A numeric value for thresholds (the context percentage).
    numeric: Option<u64>,
}

fn item_data<'a>(app: &'a App, id: &str, item: &'a Item) -> Option<ItemData<'a>> {
    let session = app.projection.read();
    match id {
        "model" => Some(ItemData {
            values: vec![
                (
                    "display_name",
                    app.model_display
                        .clone()
                        .unwrap_or_else(|| app.model_name.clone()),
                ),
                ("id", app.model_name.clone()),
                ("provider", app.provider.clone().unwrap_or_default()),
            ],
            default_format: "{display_name}",
            numeric: None,
        }),
        "reasoning" => {
            let level = app
                .next_thinking
                .as_deref()
                .or(app.thinking_level.as_deref())?;
            Some(ItemData {
                values: vec![("level", level.to_string())],
                default_format: "{level}",
                numeric: None,
            })
        }
        "tier" => {
            let tier = session.tier()?;
            Some(ItemData {
                values: vec![("tier", tier.to_string())],
                default_format: "{tier}",
                numeric: None,
            })
        }
        "cwd" => {
            let path = app.cwd.as_deref()?;
            let text = path.display().to_string();
            let home_text = std::env::var("HOME")
                .ok()
                .and_then(|home| text.strip_prefix(&home).map(|rest| format!("~{rest}")))
                .unwrap_or_else(|| text.clone());
            let base = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| text.clone());
            Some(ItemData {
                values: vec![("path", text), ("path_home", home_text), ("basename", base)],
                default_format: "{path_home}",
                numeric: None,
            })
        }
        "git_branch" => {
            let branch = app.git_branch.as_deref()?;
            Some(ItemData {
                values: vec![("branch", branch.to_string()), ("dirty", String::new())],
                default_format: "{branch}",
                numeric: None,
            })
        }
        "session" => Some(ItemData {
            values: vec![("id", app.session_id.clone())],
            default_format: "{id}",
            numeric: None,
        }),
        "context" | "context_bar" => {
            let budget = session.context_window();
            let report = session.budget();
            let used = report.map(|report| report.projected_tokens);
            let fraction = match (used, budget) {
                (Some(used), Some(budget)) if budget > 0 => used as f64 / budget as f64,
                _ => 0.0,
            };
            let percent = (fraction * 100.0).round() as u64;
            let bar = if id == "context_bar" {
                render_bar(item, fraction)
            } else {
                String::new()
            };
            Some(ItemData {
                values: vec![
                    ("bar", bar),
                    ("percent", percent.to_string()),
                    (
                        "used",
                        used.map_or_else(|| "-".to_string(), |used| used.to_string()),
                    ),
                    (
                        "budget",
                        budget.map_or_else(|| "-".to_string(), |budget| budget.to_string()),
                    ),
                ],
                default_format: if id == "context_bar" {
                    "{bar} {percent}%"
                } else {
                    "{used}/{budget}"
                },
                numeric: report.map(|_| percent),
            })
        }
        "tokens" => Some(ItemData {
            values: vec![
                (
                    "in",
                    session
                        .usage_input()
                        .map_or_else(|| "-".to_string(), |value| value.to_string()),
                ),
                (
                    "out",
                    session
                        .usage_output()
                        .map_or_else(|| "-".to_string(), |value| value.to_string()),
                ),
                ("cache", "-".to_string()),
            ],
            default_format: "↑{in} ↓{out}",
            numeric: None,
        }),
        // No cost is tracked yet: the item shows nothing rather than a fabricated number.
        "cost" => None,
        "pending" => (app.pending > 0).then(|| ItemData {
            values: vec![("count", app.pending.to_string())],
            default_format: "{count} pending",
            numeric: Some(app.pending as u64),
        }),
        "mcp" => Some(ItemData {
            values: vec![(
                "mcp",
                crate::app::mcp_text(session.mcp_configured(), session.mcp_open()),
            )],
            default_format: "{mcp}",
            numeric: None,
        }),
        "scroll" => {
            let position = if app.max_scroll == 0 {
                "all".to_string()
            } else {
                format!("{}/{}", app.scroll, app.max_scroll)
            };
            Some(ItemData {
                values: vec![
                    ("position", position),
                    (
                        "mode",
                        if app.follow { "follow" } else { "paused" }.to_string(),
                    ),
                ],
                default_format: "{position} {mode}",
                numeric: None,
            })
        }
        _ => None,
    }
}

/// Expands `{placeholders}`; an unknown placeholder is an error (the caller shows the raw
/// format; the warning was recorded at parse time).
fn expand(format: &str, data: &ItemData<'_>) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return Ok(out);
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

/// The context bar: `width` cells, `fill` for the used part and `empty` for the rest.
fn render_bar(item: &Item, fraction: f64) -> String {
    let param = |key: &str| {
        item.params
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    };
    let width: usize = param("width")
        .and_then(|value| value.parse().ok())
        .unwrap_or(10);
    let fill = param("fill").unwrap_or("━");
    let empty = param("empty").unwrap_or("─");
    let used = ((fraction.clamp(0.0, 1.0)) * width as f64).round() as usize;
    let used = used.min(width);
    format!("{}{}", fill.repeat(used), empty.repeat(width - used))
}

/// The default glyph for an item under the configured set. Only the items with a meaningful
/// icon carry one; the rest are plain text.
fn glyph(id: &str, glyphs: Glyphs) -> &'static str {
    match (id, glyphs) {
        ("reasoning", Glyphs::Unicode) => "◐ ",
        ("reasoning", Glyphs::Nerd) => "\u{f0eb} ",
        ("reasoning", Glyphs::Ascii) => "* ",
        ("git_branch", Glyphs::Nerd) => "\u{e0a0} ",
        ("session", Glyphs::Unicode) => "⛁ ",
        ("session", Glyphs::Ascii) => "# ",
        ("tier", Glyphs::Unicode) => "⬡ ",
        _ => "",
    }
}
