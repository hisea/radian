//! The shared overlay panel primitive (ADR 0035).
//!
//! A rounded box: `╭─ title ─╮` (the title embedded in the top border), rows `│ … │` (one cell
//! of padding each side), `├──┤` dividers, and `╰─ key hints ─╯` (the hints in the bottom
//! border: dim keys, muted descriptions). The pickers, the ask panel (ADR 0024), approval,
//! login and trust modals share it; only the ask panel uses it today.
//!
//! The panel is pure: it takes already-styled content rows and returns styled lines. Every color
//! comes from a theme role (ADR 0029); `glyphs = ascii` falls back to `+ - |`.
//!
//! Modeled on omp's `OverlayPanel` (`tui/src/chrome/overlay-box.ts:218-350`, MIT): the same
//! `innerWidth = max(1, width - 4)`, the same `╭─ title ─╮` / `├──┤` / `│ content │` shapes, and
//! the same "too narrow: truncate the whole block" rule (`width < 4`).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::footer::Glyphs;
use crate::theme::{Role, Theme};

/// One bottom-border key hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The key(s), e.g. `⏎` or `space`.
    pub keys: String,
    /// What the key does, e.g. `select`.
    pub description: String,
}

impl Hint {
    pub fn new(keys: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            keys: keys.into(),
            description: description.into(),
        }
    }
}

/// One row of the panel's body.
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Content(Line<'static>),
    Divider,
}

/// A panel under construction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Panel {
    title: String,
    /// An optional right-aligned label on the top border (the approval box's window
    /// indicator). Empty by default, so a plain titled panel is unchanged.
    right_title: String,
    rows: Vec<Row>,
    hints: Vec<Hint>,
}

impl Panel {
    /// A panel with `title` embedded in its top border. An empty title draws a plain top border.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// Adds one body row. The caller styles it; the panel pads and frames it.
    pub fn row(mut self, line: Line<'static>) -> Self {
        self.rows.push(Row::Content(line));
        self
    }

    /// Adds a `├──┤` divider between body rows.
    pub fn divider(mut self) -> Self {
        self.rows.push(Row::Divider);
        self
    }

    /// A right-aligned label on the top border (e.g. `lines 1–11 of 60`). Kept separate from
    /// the title so a long title and a long indicator can share the border.
    pub fn right_title(mut self, label: impl Into<String>) -> Self {
        self.right_title = label.into();
        self
    }

    /// Adds a key hint to the bottom border.
    pub fn hint(mut self, keys: impl Into<String>, description: impl Into<String>) -> Self {
        self.hints.push(Hint::new(keys, description));
        self
    }

    /// The panel's lines at `width`. `glyphs` picks the box characters; the theme supplies every
    /// color.
    pub fn lines(&self, width: usize, theme: &Theme, glyphs: Glyphs) -> Vec<Line<'static>> {
        let width = width.max(1);
        let box_chars = BoxChars::for_glyphs(glyphs);
        // Too narrow for a box (omp truncates the whole block): keep the content, drop the frame.
        if width < 4 {
            return self
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Content(line) => Some(truncate(line.clone(), width)),
                    Row::Divider => None,
                })
                .collect();
        }
        let mut out = Vec::with_capacity(self.rows.len() + 2);
        out.push(self.top_border(width, theme, &box_chars));
        for row in &self.rows {
            match row {
                Row::Content(line) => out.push(content_row(line, width, theme, &box_chars)),
                Row::Divider => out.push(divider(width, theme, &box_chars)),
            }
        }
        out.push(self.bottom_border(width, theme, &box_chars));
        // Every line is exactly `width` cells: a narrow panel clips its hints (the bottom
        // border's) rather than overflowing.
        out.into_iter().map(|line| truncate(line, width)).collect()
    }

    fn top_border(&self, width: usize, theme: &Theme, box_chars: &BoxChars) -> Line<'static> {
        let border = role_style(theme, Role::ApprovalBorder);
        let title_style = role_style(theme, Role::NoticeLabel).add_modifier(Modifier::BOLD);
        let right_style = role_style(theme, Role::Hint);
        let right = collapse(&self.right_title);
        // The right label is measured first, so the left title is truncated to leave it room.
        let right_label = if right.is_empty() {
            String::new()
        } else {
            truncate_str(&format!(" {right} "), width.saturating_sub(4))
        };
        let right_width = width_of(&right_label);
        let mut spans = vec![Span::styled(
            format!("{}{}", box_chars.top_left, box_chars.horizontal),
            border,
        )];
        let mut used = 2;
        let title = collapse(&self.title);
        if !title.is_empty() {
            let room = width.saturating_sub(3 + right_width);
            let label = truncate_str(&format!(" {title} "), room);
            used += width_of(&label);
            spans.push(Span::styled(label, title_style));
        }
        // `╭─` (2) + left + fill + right + `─` (1) + `╮` (1) == width.
        let tail = if right_width > 0 { right_width + 2 } else { 1 };
        let fill = width.saturating_sub(used + tail);
        if fill > 0 {
            spans.push(Span::styled(repeat(box_chars.horizontal, fill), border));
        }
        if right_width > 0 {
            spans.push(Span::styled(right_label, right_style));
            spans.push(Span::styled(box_chars.horizontal.to_string(), border));
        }
        spans.push(Span::styled(box_chars.top_right.to_string(), border));
        Line::from(spans)
    }

    fn bottom_border(&self, width: usize, theme: &Theme, box_chars: &BoxChars) -> Line<'static> {
        let border = role_style(theme, Role::ApprovalBorder);
        let key_style = role_style(theme, Role::Hint);
        let description_style = role_style(theme, Role::NoticeBody);
        let mut spans = vec![Span::styled(
            format!("{}{}", box_chars.bottom_left, box_chars.horizontal),
            border,
        )];
        // `╰─ ⏎ select · esc cancel ─╯`: one leading space, the keys in the dim role, a space
        // and the description in the muted role, hints joined by ` · `.
        let mut used = 0;
        for (index, hint) in self.hints.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", border));
                used += 3;
            } else {
                spans.push(Span::styled(" ", border));
                used += 1;
            }
            spans.push(Span::styled(hint.keys.clone(), key_style));
            used += width_of(&hint.keys);
            let description = format!(" {}", hint.description);
            used += width_of(&description);
            spans.push(Span::styled(description, description_style));
        }
        // `╰─` (2) + hints + fill + `╯` (1) == width.
        let fill = width.saturating_sub(used + 3);
        if fill > 0 {
            spans.push(Span::styled(repeat(box_chars.horizontal, fill), border));
        }
        spans.push(Span::styled(box_chars.bottom_right.to_string(), border));
        Line::from(spans)
    }
}

/// The box-drawing characters for one glyph set.
struct BoxChars {
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
    vertical: char,
    horizontal: char,
    tee_left: char,
    tee_right: char,
}

impl BoxChars {
    fn for_glyphs(glyphs: Glyphs) -> Self {
        match glyphs {
            Glyphs::Ascii => Self {
                top_left: '+',
                top_right: '+',
                bottom_left: '+',
                bottom_right: '+',
                vertical: '|',
                horizontal: '-',
                tee_left: '+',
                tee_right: '+',
            },
            Glyphs::Unicode | Glyphs::Nerd | Glyphs::Emoji => Self {
                top_left: '╭',
                top_right: '╮',
                bottom_left: '╰',
                bottom_right: '╯',
                vertical: '│',
                horizontal: '─',
                tee_left: '├',
                tee_right: '┤',
            },
        }
    }
}

/// One framed content row: `│ content │`, padded to the inner width.
fn content_row(
    line: &Line<'static>,
    width: usize,
    theme: &Theme,
    box_chars: &BoxChars,
) -> Line<'static> {
    let border = role_style(theme, Role::ApprovalBorder);
    let inner = width.saturating_sub(4);
    let line = if line.width() > inner {
        truncate(line.clone(), inner)
    } else {
        line.clone()
    };
    let used = line.width();
    let mut spans = vec![Span::styled(format!("{} ", box_chars.vertical), border)];
    spans.extend(line.spans);
    if used < inner {
        spans.push(Span::raw(" ".repeat(inner - used)));
    }
    spans.push(Span::styled(format!(" {}", box_chars.vertical), border));
    Line::from(spans)
}

/// A `├──┤` divider.
fn divider(width: usize, theme: &Theme, box_chars: &BoxChars) -> Line<'static> {
    let border = role_style(theme, Role::ApprovalBorder);
    Line::from(Span::styled(
        format!(
            "{}{}{}",
            box_chars.tee_left,
            repeat(box_chars.horizontal, width.saturating_sub(2)),
            box_chars.tee_right
        ),
        border,
    ))
}

/// A role's style on `theme` (the theme stays the only color source, ADR 0029).
fn role_style(theme: &Theme, role: Role) -> Style {
    let mut theme = theme.clone();
    *role.style_mut(&mut theme)
}

/// Collapses whitespace to single spaces and trims (omp's `collapseTitle`).
fn collapse(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `ch` repeated `n` times.
fn repeat(ch: char, n: usize) -> String {
    ch.to_string().repeat(n)
}

fn width_of(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Truncates a string to `max` display cells, adding `…` when it was cut.
fn truncate_str(text: &str, max: usize) -> String {
    if width_of(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('…');
    out
}

/// Truncates a `Line` to `max` display cells (spans are clipped, not dropped).
fn truncate(line: Line<'static>, max: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut width = 0;
    for span in line.spans {
        if width >= max {
            break;
        }
        let span_width = width_of(&span.content);
        if width + span_width <= max {
            width += span_width;
            spans.push(span);
        } else {
            let remaining = max - width;
            let text = truncate_str(&span.content, remaining);
            spans.push(Span::styled(text, span.style));
            break;
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }

    fn render(panel: &Panel, width: usize) -> Vec<String> {
        panel
            .lines(width, &theme(), Glyphs::Unicode)
            .into_iter()
            .map(|line| line.to_string())
            .collect()
    }

    /// The frame: a titled top border, framed rows, a `├──┤` divider and a bottom border with
    /// the hints. Every line is exactly `width` cells.
    #[test]
    fn the_frame_is_a_titled_rounded_box() {
        let panel = Panel::new("Ask 2 questions")
            .row(Line::from("first"))
            .divider()
            .row(Line::from("second"))
            .hint("⏎", "select")
            .hint("esc", "cancel");
        let lines = render(&panel, 40);
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(
            lines[0].starts_with("╭─ Ask 2 questions "),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].ends_with('╮'), "{:?}", lines[0]);
        assert_eq!(lines[1], "│ first                                │");
        assert_eq!(lines[2], "├──────────────────────────────────────┤");
        assert_eq!(lines[3], "│ second                               │");
        assert!(
            lines[4].starts_with("╰─ ⏎ select · esc cancel"),
            "{:?}",
            lines[4]
        );
        assert!(lines[4].ends_with('╯'), "{:?}", lines[4]);
        for line in &lines {
            assert_eq!(width_of(line), 40, "{line:?}");
        }
    }

    /// An empty title draws a plain top border; whitespace in a title collapses.
    #[test]
    fn the_title_collapses_and_may_be_empty() {
        let lines = render(&Panel::new(""), 20);
        assert_eq!(lines[0], "╭──────────────────╮");
        let lines = render(&Panel::new("  a \n b  "), 30);
        assert!(lines[0].starts_with("╭─ a b "), "{:?}", lines[0]);
    }

    /// A right title shares the top border: `╭─ title ── right ─╮`, exactly `width` cells.
    #[test]
    fn a_right_title_sits_on_the_top_border() {
        let panel = Panel::new("approve tool call?").right_title("lines 1–11 of 60");
        let lines = render(&panel, 60);
        assert!(
            lines[0].starts_with("╭─ approve tool call? "),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].ends_with(" lines 1–11 of 60 ─╮"), "{:?}", lines[0]);
        assert_eq!(width_of(&lines[0]), 60, "{:?}", lines[0]);
        // A right title alone keeps the border shape.
        let lines = render(&Panel::new("").right_title("x"), 20);
        assert!(lines[0].starts_with("╭"), "{:?}", lines[0]);
        assert!(lines[0].ends_with(" x ─╮"), "{:?}", lines[0]);
        assert_eq!(width_of(&lines[0]), 20, "{:?}", lines[0]);
    }

    /// The ascii glyph set uses `+ - |`.
    #[test]
    fn ascii_falls_back() {
        let panel = Panel::new("T").row(Line::from("x")).hint("q", "quit");
        let lines: Vec<String> = panel
            .lines(20, &theme(), Glyphs::Ascii)
            .into_iter()
            .map(|line| line.to_string())
            .collect();
        assert!(lines[0].starts_with("+- T "), "{:?}", lines[0]);
        assert!(lines[1].starts_with("| x"), "{:?}", lines[1]);
        assert!(lines[1].ends_with(" |"), "{:?}", lines[1]);
        assert_eq!(width_of(&lines[1]), 20, "{:?}", lines[1]);
        assert!(lines[2].starts_with("+- q quit"), "{:?}", lines[2]);
        assert!(!lines.iter().any(|line| line.contains('╭')), "{lines:?}");
    }

    /// Too narrow for a box (`width < 4`): the frame is dropped and the content is truncated.
    #[test]
    fn a_tiny_width_drops_the_frame() {
        let panel = Panel::new("T").row(Line::from("abcdef")).divider();
        let lines = render(&panel, 3);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(width_of(&lines[0]), 3, "{lines:?}");
    }

    /// A long title is truncated with `…`, and a long row is clipped to the inner width.
    #[test]
    fn long_content_is_truncated() {
        let lines = render(
            &Panel::new("a very long title indeed").row(Line::from("x".repeat(60))),
            24,
        );
        assert!(lines[0].contains('…'), "{:?}", lines[0]);
        assert_eq!(width_of(&lines[0]), 24, "{:?}", lines[0]);
        assert_eq!(width_of(&lines[1]), 24, "{:?}", lines[1]);
        assert!(lines[1].ends_with('│'), "{:?}", lines[1]);
    }
}
