//! The input area: the input box (its style, height, inner width and glyphs), the running status
//! drawn on the top rule, and the `@` completion menu above it.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::App;

/// The tallest the input area grows before it scrolls internally instead.
const MAX_INPUT_ROWS: usize = 10;

/// The rows the find box needs: its top border, the query, its bottom border (`Borders::ALL`).
pub(super) const FIND_BOX_HEIGHT: u16 = 3;

/// How the input area is drawn (config `tui.input_style`).
///
/// The `tui.input_style` values, in documentation order. The parser and the manual's drift
/// test both read this list.
pub const INPUT_STYLES: &[&str] = &["lines", "box"];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputStyle {
    /// A rule above and below, `❯ ` on the first line, continuation lines indented two cells
    /// (ADR 0035: the default).
    #[default]
    Lines,
    /// The thin half frame (ADR 0035, the 2026-10-16 note): an **open**
    /// frame shaped like a code block's — a full-width top rule (idle) or a status label
    /// (running), each input row `│ ❯ text` (`│   text` for a continuation), and a `╰──` tail.
    Box,
}

impl InputStyle {
    /// The config spelling. `None` for anything else, so the caller falls back to the default.
    /// `bar` was removed (ADR 0035's 2026-10-16 note); the caller warns once and uses `lines`.
    pub fn parse(name: &str) -> Option<Self> {
        if !INPUT_STYLES.contains(&name) {
            return None;
        }
        match name {
            "lines" => Some(Self::Lines),
            "box" => Some(Self::Box),
            _ => None,
        }
    }

    /// The `tui.input_style` spelling of this style.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lines => INPUT_STYLES[0],
            Self::Box => INPUT_STYLES[1],
        }
    }
}

/// The input prompt, in the style of pi: the first line starts with `❯ ` and the continuation
/// lines indent by the same two cells, so the text stays aligned.
/// decision: **both** input styles use `❯`. Its display width (2 cells) is what the layout
/// reserves — never its byte length (`❯` is three bytes, which used to shift the box cursor).
pub(super) const INPUT_PROMPT: &str = "❯ ";

/// The `box` style's prompt: the `❯`, colored with the theme's
/// accent role (`theme.notice_label`). Two cells, like `> `.
pub(super) const BOX_PROMPT: &str = "❯ ";

/// The placeholder shown while the input is empty.
pub(super) const INPUT_HINT: &str = "(type a message, Enter to send, Ctrl-J for a new line)";

/// The placeholder while the input is empty and no model is routed (ADR 0008 §2).
pub(super) const INPUT_HINT_NO_MODEL: &str =
    "(no model set — choose one with /model, Enter to send)";

/// The `box` style's thin half frame (ADR 0035, the 2026-10-16 note): the
/// same open frame a code block draws — a top rule, a `│ ` rail on every row and a `╰──` tail —
/// chosen from the terminal's glyph set (`ascii` uses `+`, `-`, `| `, `+--`).
pub(super) struct BoxGlyphs {
    /// The top-left corner.
    pub(super) open: &'static str,
    /// The rule fill.
    pub(super) rule: char,
    /// The body rail (two cells: the bar and its space).
    pub(super) rail: &'static str,
    /// The bottom tail (three cells).
    pub(super) bottom: &'static str,
}

impl BoxGlyphs {
    pub(super) fn for_glyphs(glyphs: crate::footer::Glyphs) -> Self {
        match glyphs {
            crate::footer::Glyphs::Ascii => Self {
                open: "+",
                rule: '-',
                rail: "| ",
                bottom: "+--",
            },
            _ => Self {
                open: "╭",
                rule: '─',
                rail: "│ ",
                bottom: "╰──",
            },
        }
    }
}

/// The input area's height: the text rows plus the style's chrome — the top rule and the bottom
/// tail (ADR 0035 / the 2026-10-16 note).
pub(super) fn input_height(app: &App, width: usize) -> u16 {
    let inner = input_inner_width(app, width);
    let rows = crate::composer::rows(&app.input.text(), inner)
        .len()
        .clamp(1, MAX_INPUT_ROWS);
    // One row per composer image attachment, above the text rows.
    (rows + app.composer_images.len()) as u16 + 2
}

/// The input's visible text rows (`input_area.height` minus the two chrome rows and the image
/// attachment rows), at least one.
pub(super) fn input_visible_rows(app: &App, height: u16) -> usize {
    height
        .saturating_sub(2 + app.composer_images.len() as u16)
        .max(1) as usize
}

/// The display width of a prompt constant: the columns it occupies, **not** its byte length
/// (`❯` is three bytes and one cell). Every layout use of a prompt goes through this.
pub(super) fn prompt_width(prompt: &str) -> usize {
    UnicodeWidthStr::width(prompt)
}

/// The text width inside the input area: `❯ ` on the first line for `lines`; the `│ ` body rail
/// plus the `❯ ` prompt for `box`.
pub(super) fn input_inner_width(app: &App, width: usize) -> usize {
    let reserved = match app.input_style {
        InputStyle::Lines => prompt_width(INPUT_PROMPT),
        // The body rail (`│ `) plus the `❯ ` prompt.
        InputStyle::Box => 2 + prompt_width(BOX_PROMPT),
    };
    width.saturating_sub(reserved).max(1)
}

/// The running input's status content: `<spinner> Working · 12s · esc to interrupt`, or `None`
/// while idle (ADR 0027, `App::spinner_status`). The spinner takes the pulse style;
/// the word, clock and hint take the dim/hint level. The `box` top border draws it where a code
/// block's language label sits (ADR 0035's 2026-10-16 note). The clock is the footer's `elapsed`
/// (`TurnTimer`: the running generation, else the last one), written in whole seconds as the
/// ADR's `12s`; `esc to interrupt` is shown exactly while a turn runs, because that is when Esc
/// cancels it.
pub(super) fn input_status_spans(app: &App) -> Option<Vec<Span<'static>>> {
    let status = app.spinner_status()?;
    let spin = app.spinner_frame();
    // The frame is never bold (ADR 0035's 2026-10-16 note): the pulse keeps the spinner role's
    // colour but drops the accent's BOLD weight.
    let spinner = app.spinner_style(spin).remove_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled(spin.glyph, spinner),
        Span::styled(format!(" {status}"), app.theme.hint),
    ];
    if let Some(elapsed) = app.session.timer.elapsed(app.now) {
        spans.push(Span::styled(
            format!(" · {}", crate::timing::format_seconds(elapsed)),
            app.theme.hint,
        ));
    }
    if app.turn_running() {
        spans.push(Span::styled(" · esc to interrupt", app.theme.hint));
    }
    Some(spans)
}

/// The most rows the completion menu shows (omp's `#autocompleteMaxVisible`).
pub const COMPLETION_MAX_LINES: usize = 10;

/// The completion menu's visible rows: omp's `max(3, min(10, viewport - used - 2))`. Zero when
/// no menu is up.
pub fn completion_visible_rows(app: &App, viewport: usize, used: usize) -> usize {
    if app.has_modal() || app.completion().is_none() {
        return 0;
    }
    viewport
        .saturating_sub(used + 2)
        .clamp(3, COMPLETION_MAX_LINES)
}

/// The completion menu's rows (ADR 0035): the shared list view, so the window
/// follows the highlight and the overflow shows a scrollbar instead of a `+N more` line.
pub fn completion_rows(app: &App, width: usize, visible: usize) -> Vec<Line<'static>> {
    if app.has_modal() {
        return Vec::new();
    }
    let Some(candidates) = app.completion() else {
        return Vec::new();
    };
    let items: Vec<crate::select_list::Item> = candidates
        .iter()
        .map(|candidate| {
            let item = crate::select_list::Item::new(candidate.insert.clone())
                .description(candidate.description.clone());
            if candidate.current {
                item.current()
            } else {
                item
            }
        })
        .collect();
    crate::select_list::view(
        &app.theme,
        &items,
        &app.completion_filter(),
        app.completion_cursor,
        visible,
        width,
        false,
    )
    .lines
}
