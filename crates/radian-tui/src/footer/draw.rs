//! The footer's draw path (moved from `footer.rs` by pure move, ADR 0043 decision 5): row
//! rendering with truncation, item dispatch through the component registry, placeholder
//! expansion, and the shared style/contribution helpers the components read.

use radian_host::SegmentRole;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::theme::Role;

use super::components;
use super::components::{Component, ItemData};
use super::{ContextLine, Glyphs, Item, ItemStyle, Row, parse_item_style};

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

pub(crate) fn render_row(
    app: &App,
    row: &Row,
    width: usize,
    context_line: ContextLine,
) -> Line<'static> {
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

pub(crate) fn role_style(app: &App, role: Role) -> Style {
    let mut theme = app.theme.clone();
    *role.style_mut(&mut theme)
}

/// One store item's joined text, for the items that only need the value (`cwd`'s `{repo}`).
pub(crate) fn contribution_text(app: &App, item: &str) -> Option<String> {
    contribution(app, item).map(|(text, _, _)| text)
}

/// One store item as (joined text, first role, stale).
pub(crate) fn contribution(app: &App, item: &str) -> Option<(String, SegmentRole, bool)> {
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
pub(crate) fn contrib_role(role: SegmentRole) -> Role {
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
pub(crate) fn auto_marker(app: &App) -> String {
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
pub(crate) fn inline_styles(format: &str) -> Vec<String> {
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
