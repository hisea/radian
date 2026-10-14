//! Assistant-text Markdown (ADR 0028 R2): blocks, inline roles, tables, the streaming case,
//! selection copy and search. `TestBackend` frames, so colors and modifiers are asserted on
//! real cells.

use radian_tui::app::{App, EntryKind};
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::render;
use radian_tui::select;
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn modes(reasoning: ReasoningMode) -> DisplayModes {
    DisplayModes {
        reasoning,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
    }
}

fn assistant(app: &mut App, text: &str) {
    app.entries
        .push(radian_tui::app::Entry::new(EntryKind::Assistant, text));
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn style_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<ratatui::style::Style> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut line = String::new();
    for y in area.top()..area.bottom() {
        line.clear();
        for x in area.left()..area.right() {
            line.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        if let Some(col) = line.find(needle) {
            let x = area.left() + col as u16;
            return buffer.cell((x, y)).map(|c| c.style());
        }
    }
    None
}

/// Every block kind and inline role reaches the screen with its own styling.
#[test]
fn the_block_kinds_and_inline_roles_render() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "# Title\n\nA **bold** *italic* `code` [link](https://x.example) word.\n\n- one\n  - nested\n- two\n\n> quoted\n\n---\n",
    );
    let terminal = render(&mut app, 60, 24);
    let text = screen(&terminal);
    assert!(text.contains("Title"), "{text}");
    assert!(
        text.contains("bold") && !text.contains("**bold**"),
        "{text}"
    );
    assert!(text.contains("italic"), "{text}");
    assert!(text.contains("code"), "{text}");
    assert!(text.contains("link (https://x.example)"), "{text}");
    assert!(text.contains("• one"), "{text}");
    assert!(text.contains("  • nested"), "nested indent: {text}");
    assert!(text.contains("│ quoted"), "{text}");
    assert!(text.contains('─'), "the rule: {text}");
    // The roles' colors come from the theme.
    let heading = style_of(&terminal, "Title").expect("heading");
    let link = style_of(&terminal, "link").expect("link");
    let code = style_of(&terminal, "code").expect("code");
    let bullet = style_of(&terminal, "•").expect("bullet");
    assert_eq!(heading.fg, app.theme.md_heading.fg);
    assert_eq!(link.fg, app.theme.md_link.fg);
    assert_eq!(code.fg, app.theme.md_code.fg);
    assert_eq!(bullet.fg, app.theme.md_list_bullet.fg);
    assert_ne!(heading.fg, link.fg, "roles differ");
}

/// A table draws borders when it fits, wraps a cell that does not, and falls back to the raw
/// `|` text when there are too many columns.
#[test]
fn tables_wrap_and_fall_back() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "| one | two |\n| --- | --- |\n| a very long cell that must wrap | b |\n",
    );
    let text = screen(&render(&mut app, 30, 16));
    assert!(
        text.contains('┌') && text.contains('│'),
        "bordered table: {text}"
    );
    assert!(text.contains("one"), "{text}");
    // Too many columns for the width: the raw `|` source comes back.
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "| a | b | c | d | e | f | g | h |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |\n",
    );
    let text = screen(&render(&mut app, 20, 16));
    assert!(
        text.contains("| a | b |"),
        "the fallback keeps the raw source: {text}"
    );
    assert!(!text.contains('┌'), "no partial table: {text}");
}

/// A half-typed list and a half table still show something sane; they fix up when closed.
#[test]
fn a_streaming_half_parses() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "- one\n- tw");
    let text = screen(&render(&mut app, 40, 12));
    assert!(text.contains("• one") && text.contains("• tw"), "{text}");
    let mut app = App::new("m", "s");
    assistant(&mut app, "| a | b |\n| --- | --- |\n| 1 |");
    let text = screen(&render(&mut app, 40, 12));
    assert!(text.contains('┌') || text.contains("| a |"), "{text}");
}

/// A selection across bold and a link copies the **source** it covers (ADR 0028 R2).
#[test]
fn a_selection_copies_the_source_it_covers() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "A **bold** [link](https://x) tail\n");
    app.set_transcript_width(60);
    // Row 0 is the paragraph: cells 2..=9 are `bold** [l` (the display text is
    // `A bold link (https://x) tail`).
    let anchor =
        select::locate(&app.entries, modes(ReasoningMode::Expanded), 60, 0, 2).expect("anchor");
    let head =
        select::locate(&app.entries, modes(ReasoningMode::Expanded), 60, 0, 9).expect("head");
    app.selection = Some(select::Selection { anchor, head });
    let text = radian_tui::select::logical_text(&app.entries, &app.selection.expect("a selection"));
    assert!(
        text.contains("**bold**"),
        "the copy includes the markup: {text:?}"
    );
}

/// Ctrl-F matches the displayed text (not the markup).
#[test]
fn find_matches_the_displayed_text() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "A **bold** and a [link](https://x).\n");
    let (matches, _) =
        radian_tui::find::compute(&app.entries, modes(ReasoningMode::Expanded), 60, "bold");
    assert_eq!(matches.len(), 1, "the displayed word matches");
    let (matches, _) =
        radian_tui::find::compute(&app.entries, modes(ReasoningMode::Expanded), 60, "**bold**");
    assert!(matches.is_empty(), "the markup is not displayed");
}

/// `line_count` equals the drawn body lines at two widths (virtualization).
#[test]
fn line_count_matches_the_layout() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "# Title\n\nA paragraph that is long enough to wrap at a narrow width for sure.\n\n- one\n- two\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n",
    );
    let entry = &app.entries[0];
    let blocks = blocks::parse_blocks(entry.text.as_str());
    for width in [60usize, 20] {
        let count = blocks::line_count(entry, modes(ReasoningMode::Expanded), width);
        let laid_out = blocks::layout(entry, &blocks, modes(ReasoningMode::Expanded), width).len();
        assert_eq!(count, laid_out, "width {width}");
    }
}
