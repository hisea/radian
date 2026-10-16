//! The shared list view (ADR 0035, omp's `SelectList`): a `❯ ` cursor, two columns
//! when the terminal is wide enough, a scroll window that follows the selection, and a
//! one-column scrollbar when the list overflows. Pure: items, filter and geometry in, lines
//! out. The completion menu, `/model`, `/theme`, `/thinking` and the resume picker draw
//! through it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

/// One row's data. `description` is the second column (a provider name, a command's summary);
/// it is only drawn when the terminal is wide enough.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub description: Option<String>,
    /// The item in effect (the current model, theme or thinking level): a ` ✓` is drawn right
    /// after its label (ASCII tier ` *`).
    pub current: bool,
}

impl Item {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: None,
            current: false,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn current(mut self) -> Self {
        self.current = true;
        self
    }
}

/// The cursor glyph: `❯ ` (ASCII falls back to `> `).
pub fn cursor(ascii: bool) -> &'static str {
    if ascii { "> " } else { "❯ " }
}

/// Whether an item matches a filter: a case-insensitive substring of the label **or** the
/// description (ADR 0035: typing `opencode` finds that provider's models).
pub fn matches(item: &Item, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let needle = filter.to_lowercase();
    item.label.to_lowercase().contains(&needle)
        || item
            .description
            .as_deref()
            .is_some_and(|description| description.to_lowercase().contains(&needle))
}

/// The filtered item indices, in order.
pub fn filtered(items: &[Item], filter: &str) -> Vec<usize> {
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| matches(item, filter))
        .map(|(index, _)| index)
        .collect()
}

/// The index (into the filtered list) of the first visible row: a window of `visible` rows
/// that keeps `selected` in view. The window only moves when the selection leaves it, so the
/// list does not jump on every keystroke.
pub fn window(len: usize, selected: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    if len <= visible {
        return 0;
    }
    let selected = selected.min(len - 1);
    if selected < visible {
        0
    } else {
        selected + 1 - visible
    }
}

/// The rendered list.
#[derive(Debug, Clone)]
pub struct ListView {
    pub lines: Vec<Line<'static>>,
    /// The index (into `items`) of the first visible row.
    pub start: usize,
    /// `(1-based position, total)` of the selected row among the filtered items.
    pub position: Option<(usize, usize)>,
    /// The filter matched nothing.
    pub empty: bool,
    /// The list overflowed: the last column of each row is the scrollbar.
    pub overflow: bool,
}

/// The two-column split (omp's numbers): the description column exists only above 40 columns.
const PRIMARY_COLUMN: usize = 32;
const MIN_DESCRIPTION: usize = 10;

/// The current-item mark's display width: ` ✓` (ASCII ` *`), never part of the description.
const CURRENT_MARK_WIDTH: usize = 2;

/// Renders the list. `selected` indexes the **filtered** rows (the caller resets it to 0 when
/// the filter changes). `visible` is how many item rows fit; the caller computes it (omp's
/// formula is `max(3, min(10, viewport_rows - used_rows - 2))`).
pub fn view(
    theme: &Theme,
    items: &[Item],
    filter: &str,
    selected: usize,
    visible: usize,
    width: usize,
    ascii: bool,
) -> ListView {
    let indices = filtered(items, filter);
    let total = indices.len();
    let empty = total == 0;
    let visible = visible.max(1);
    let selected = selected.min(total.saturating_sub(1));
    let start = window(total, selected, visible);
    let overflow = total > visible;
    let two_columns = width > 40;
    let track_width = usize::from(overflow);
    let content_width = width.saturating_sub(track_width);
    // The label column fits the longest filtered label (cursor, label, the current-item mark,
    // one space), so the description column is the same for every row and does not move while
    // scrolling. A longer label is cut at omp's 32 columns, or at half the width when that is
    // wider. The current item's mark counts as two columns, so a long current label is cut to
    // make room for it instead of pushing its description.
    let longest = indices
        .iter()
        .map(|&index| {
            let item = &items[index];
            UnicodeWidthStr::width(item.label.as_str())
                + if item.current { CURRENT_MARK_WIDTH } else { 0 }
        })
        .max()
        .unwrap_or(0);
    let label_column = (longest + 3).min(PRIMARY_COLUMN.max(content_width / 2));
    let mut lines = Vec::new();
    for row in 0..visible {
        let position = start + row;
        if position >= total {
            break;
        }
        let item = &items[indices[position]];
        let is_selected = position == selected;
        lines.push(row_line(
            theme,
            item,
            is_selected,
            content_width,
            label_column,
            two_columns,
            ascii,
        ));
    }
    if overflow {
        let thumb = scrollbar_thumb(total, visible, start, lines.len());
        for (row, line) in lines.iter_mut().enumerate() {
            // The track sits in one column for every row, whatever the row's own width.
            let drawn = line.width();
            if drawn < content_width {
                line.spans
                    .push(Span::raw(" ".repeat(content_width - drawn)));
            }
            let cell = if row == thumb { '█' } else { '│' };
            let cell = if ascii {
                if row == thumb { '#' } else { '|' }
            } else {
                cell
            };
            let style = if row == thumb {
                theme.completion.add_modifier(Modifier::BOLD)
            } else {
                theme.truncation
            };
            line.spans.push(Span::styled(cell.to_string(), style));
        }
    }
    ListView {
        lines,
        start: if total == 0 { 0 } else { indices[start] },
        position: (!empty).then_some((selected + 1, total)),
        empty,
        overflow,
    }
}

/// One item row: the cursor, the label, the current-item mark and the description column. The
/// current item is marked ` ✓` right after its label (ASCII ` *`) in the `status_ok` role; the
/// mark is never part of the description, so every row's description starts in the same column
///. The whole row never exceeds `width` display columns.
fn row_line(
    theme: &Theme,
    item: &Item,
    is_selected: bool,
    width: usize,
    label_column: usize,
    two_columns: bool,
    ascii: bool,
) -> Line<'static> {
    let selected_style = if is_selected {
        theme.completion.add_modifier(Modifier::BOLD)
    } else {
        theme.completion
    };
    let cursor = if is_selected { cursor(ascii) } else { "  " };
    let mut spans = vec![Span::styled(cursor.to_string(), selected_style)];
    let mut used = 2usize;
    let marker = if item.current {
        if ascii { " *" } else { " ✓" }
    } else {
        ""
    };
    let marker_width = UnicodeWidthStr::width(marker);
    let description = item.description.as_deref().unwrap_or("");
    if two_columns && !description.is_empty() && width > 2 {
        // The label ends before the description column, leaving room for the current mark, so a
        // long current label is cut before the mark and the description column never moves.
        let label = truncate(
            &item.label,
            label_column
                .saturating_sub(used + marker_width + 1)
                .min(width.saturating_sub(used + marker_width)),
        );
        used += UnicodeWidthStr::width(label.as_str());
        spans.push(Span::styled(label, selected_style));
        used += draw_marker(&mut spans, marker, theme.status_ok, used, width);
        let pad = label_column
            .saturating_sub(used)
            .max(1)
            .min(width.saturating_sub(used));
        spans.push(Span::raw(" ".repeat(pad)));
        used += pad;
        let room = width.saturating_sub(used);
        // The description column only exists when it can hold a useful amount.
        if room >= MIN_DESCRIPTION {
            let description = truncate(description, room);
            spans.push(Span::styled(description, theme.truncation));
        }
    } else {
        let label = truncate(&item.label, width.saturating_sub(used + marker_width));
        used += UnicodeWidthStr::width(label.as_str());
        spans.push(Span::styled(label, selected_style));
        draw_marker(&mut spans, marker, theme.status_ok, used, width);
    }
    Line::from(spans)
}

/// Draws the current-item mark right after its label when the row is wide enough for the whole
/// glyph, returning the columns it used. A row too narrow keeps its (already truncated) label
/// and drops the mark; the mark itself is never cut to a partial glyph.
fn draw_marker(
    spans: &mut Vec<Span<'static>>,
    marker: &str,
    style: Style,
    used: usize,
    width: usize,
) -> usize {
    let marker_width = UnicodeWidthStr::width(marker);
    if marker.is_empty() || used + marker_width > width {
        return 0;
    }
    spans.push(Span::styled(marker.to_string(), style));
    marker_width
}

/// The scrollbar thumb's row: proportional to the window's position in the list, always a
/// valid row of the drawn window.
fn scrollbar_thumb(total: usize, visible: usize, start: usize, rows: usize) -> usize {
    if rows <= 1 || total <= visible {
        return 0;
    }
    let max_start = total.saturating_sub(visible).max(1);
    let row = (start * (rows - 1) + max_start / 2) / max_start;
    row.min(rows - 1)
}

/// Cuts `text` to at most `width` display columns, never splitting a wide character.
fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > width {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn items(n: usize) -> Vec<Item> {
        (0..n).map(|i| Item::new(format!("item {i:02}"))).collect()
    }

    fn text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn the_window_follows_the_selection_and_shows_a_scrollbar() {
        let theme = Theme::default();
        // 30 items, 10 visible: moving to 14 keeps it in the window and moves the thumb.
        let list = view(&theme, &items(30), "", 14, 10, 80, false);
        let rows: Vec<String> = list.lines.iter().map(text).collect();
        assert_eq!(rows.len(), 10);
        assert!(
            rows.iter().any(|row| row.contains("item 14")),
            "the selection must be drawn: {rows:?}"
        );
        assert_eq!(list.start, 5, "the window starts at 5");
        assert!(list.overflow);
        assert!(rows[0].ends_with('│') || rows[0].ends_with('█'));
        // The thumb is proportional: at the top it is the first row, at the bottom the last.
        let top = view(&theme, &items(30), "", 0, 10, 80, false);
        assert!(text(&top.lines[0]).ends_with('█'));
        let bottom = view(&theme, &items(30), "", 29, 10, 80, false);
        assert!(text(bottom.lines.last().unwrap()).ends_with('█'));
        assert_eq!(bottom.position, Some((30, 30)));
    }

    #[test]
    fn a_short_list_has_no_scrollbar_and_no_cursor_move() {
        let theme = Theme::default();
        let list = view(&theme, &items(3), "", 1, 10, 80, false);
        assert_eq!(list.lines.len(), 3);
        assert!(!list.overflow);
        assert_eq!(list.start, 0);
        assert!(list.lines.iter().all(|line| !text(line).ends_with('│')));
    }

    #[test]
    fn the_filter_matches_the_label_and_the_description() {
        let theme = Theme::default();
        let items = vec![
            Item::new("deepseek-v4.1-flash").description("opencode-go"),
            Item::new("gpt-5").description("openai"),
        ];
        assert_eq!(filtered(&items, "opencode"), vec![0]);
        assert_eq!(filtered(&items, "gpt"), vec![1]);
        assert_eq!(filtered(&items, "OPencode"), vec![0]);
        assert!(filtered(&items, "nope").is_empty());
        let view = view(&theme, &items, "opencode", 0, 10, 80, false);
        assert_eq!(view.position, Some((1, 1)));
        assert!(
            view.lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("deepseek"))
        );
    }

    #[test]
    fn a_narrow_terminal_is_one_column_and_ascii_uses_the_ascii_glyphs() {
        let theme = Theme::default();
        let entries = vec![Item::new("deepseek-v4.1-flash").description("opencode-go")];
        let narrow = view(&theme, &entries, "", 0, 10, 30, false);
        let row = text(&narrow.lines[0]);
        assert!(row.starts_with("❯ "), "{row:?}");
        assert!(
            !row.contains("opencode-go"),
            "no description column: {row:?}"
        );
        let ascii = view(&theme, &entries, "", 0, 10, 30, true);
        assert!(text(&ascii.lines[0]).starts_with("> "));
        let many = view(&theme, &items(30), "", 0, 10, 30, true);
        // The thumb is at the top (row 0), the rest is the ascii track.
        assert!(text(&many.lines[0]).ends_with('#'), "ascii thumb");
        assert!(text(&many.lines[1]).ends_with('|'), "ascii track");
    }

    /// The scrollbar is one column for every row, and a long label leaves the description
    /// column where it is.
    #[test]
    fn the_track_and_the_description_column_line_up() {
        let theme = Theme::default();
        let mut entries: Vec<Item> = (0..12)
            .map(|i| Item::new(format!("opencode-go/m{i}")).description("opencode-go"))
            .collect();
        entries[1] =
            Item::new("opencode-go/deepseek-v4-flash-vision-exp").description("opencode-go");
        entries[2] = Item::new("opencode-go/deepseek-v4.1-flash")
            .description("opencode-go")
            .current();
        let list = view(&theme, &entries, "", 0, 10, 80, false);
        assert!(list.overflow);
        let rows: Vec<String> = list.lines.iter().map(text).collect();
        for row in &rows {
            assert_eq!(UnicodeWidthStr::width(row.as_str()), 80, "{row:?}");
        }
        // The display column where the description starts. The last `opencode-go` on a row is the
        // description; every row's label is longer and comes earlier.
        let column = |row: &str| {
            let at = row.rfind("opencode-go").unwrap();
            UnicodeWidthStr::width(&row[..at])
        };
        let first = column(&rows[0]);
        assert_eq!(
            column(&rows[1]),
            first,
            "a long label moved the description: {rows:?}"
        );
        assert_eq!(column(&rows[2]), first, "{rows:?}");
        assert!(
            rows[2].contains("opencode-go/deepseek-v4.1-flash ✓"),
            "the current label is drawn in full before its mark: {rows:?}"
        );
        assert!(
            !rows.iter().any(|row| row.contains("current")),
            "the description never carries the marker: {rows:?}"
        );
    }

    /// the current item is marked ` ✓` right after its label, never in the
    /// description; the description still starts in the same column on every row.
    #[test]
    fn the_current_item_is_marked_after_its_label() {
        let theme = Theme::default();
        let entries = vec![
            Item::new("dark").description("built-in theme"),
            Item::new("mono").description("built-in theme").current(),
            Item::new("light").description("built-in theme"),
        ];
        let list = view(&theme, &entries, "", 0, 10, 80, false);
        let rows: Vec<String> = list.lines.iter().map(text).collect();
        assert!(
            rows[1].contains("mono ✓"),
            "the current row is marked after its label: {rows:?}"
        );
        for (index, row) in rows.iter().enumerate() {
            assert!(!row.contains("current"), "{row:?}");
            if index != 1 {
                assert!(
                    !row.contains('✓'),
                    "only the current row is marked: {row:?}"
                );
            }
        }
        let column = |row: &str| UnicodeWidthStr::width(&row[..row.rfind("built-in").unwrap()]);
        assert_eq!(column(&rows[0]), column(&rows[1]), "{rows:?}");
        assert_eq!(column(&rows[1]), column(&rows[2]), "{rows:?}");
    }

    /// ASCII tier: the mark is ` *`.
    #[test]
    fn the_ascii_tier_marks_the_current_item_with_a_star() {
        let theme = Theme::default();
        let entries = vec![
            Item::new("dark").description("built-in theme"),
            Item::new("mono").description("built-in theme").current(),
        ];
        let list = view(&theme, &entries, "", 0, 10, 80, true);
        let rows: Vec<String> = list.lines.iter().map(text).collect();
        assert!(rows[1].contains("mono *"), "{rows:?}");
        assert!(!rows.iter().any(|row| row.contains('✓')), "{rows:?}");
    }

    /// A current label longer than the column is cut before the ` ✓`; the mark stays and the
    /// description column does not move.
    #[test]
    fn a_long_current_label_is_truncated_before_its_mark() {
        let theme = Theme::default();
        let long = "a-very-long-current-label-that-does-not-fit-the-column";
        let entries = vec![
            Item::new("short").description("provider"),
            Item::new(long).description("provider").current(),
        ];
        let list = view(&theme, &entries, "", 0, 10, 80, false);
        let rows: Vec<String> = list.lines.iter().map(text).collect();
        assert!(!rows[1].contains(long), "the label is cut: {rows:?}");
        assert!(
            rows[1].contains('✓'),
            "the mark stays after the label is cut: {rows:?}"
        );
        assert!(
            UnicodeWidthStr::width(rows[1].as_str()) <= 80,
            "the row never exceeds the width: {rows:?}"
        );
        let column = |row: &str| UnicodeWidthStr::width(&row[..row.rfind("provider").unwrap()]);
        assert_eq!(column(&rows[1]), column(&rows[0]), "{rows:?}");
    }

    #[test]
    fn an_empty_filter_match_is_marked() {
        let theme = Theme::default();
        let list = view(&theme, &items(3), "zzz", 0, 10, 80, false);
        assert!(list.empty);
        assert!(list.lines.is_empty());
        assert_eq!(list.position, None);
    }
}
