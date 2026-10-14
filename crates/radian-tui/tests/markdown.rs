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
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

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

/// Locate the cell of the `n`-th character of the **displayed** body (across the wrapped
/// lines), as `(line, column)`.
fn cell_of(lines: &[radian_tui::blocks::BodyLine], n: usize) -> Option<(usize, usize)> {
    let mut seen = 0usize;
    for (line_index, line) in lines.iter().enumerate() {
        let cells = line.chars.len();
        if n < seen + cells {
            return Some((line_index, n - seen));
        }
        seen += cells;
    }
    None
}

/// The property (review R2): the selection mapping points at the **source** characters. Every
/// display cell that came from the source (`md` piece with an offset) locates to exactly that
/// source offset, at every block kind and width; a synthesized cell (a bullet, a link's
/// ` (url)`) locates to its block's start and is skipped here.
#[test]
fn every_cell_maps_to_its_source_offset() {
    let bodies = [
        "alpha beta gamma delta epsilon\n",
        "A **bold** *italic* `code` [link](https://x) tail\n",
        "first paragraph\n\nsecond paragraph\n",
        "- item one\n- item two\n\n> quoted\n",
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n",
        "# Heading\n\nbody text\n",
    ];
    for body in bodies {
        for width in [80usize, 30, 16] {
            let mut app = App::new("m", "s");
            assistant(&mut app, body);
            let modes = modes(ReasoningMode::Expanded);
            let blocks = blocks::parse_blocks(body);
            let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
            let cells: usize = lines.iter().map(|line| line.chars.len()).sum();
            for cell in 0..cells {
                let (line_index, column) = cell_of(&lines, cell).expect("a cell");
                // `layout` is body-relative; `locate` takes screen columns (the assistant
                // prefix is two cells).
                let anchor = select::locate(&app.entries, modes, width, line_index, column + 2)
                    .expect("an anchor");
                assert_eq!(
                    anchor.offset, lines[line_index].chars[column],
                    "body {body:?} width {width} cell {cell}: the anchor must be the cell's \
                     source offset"
                );
            }
        }
    }
}

/// The soft-wrap case, spelled out: selecting across the break copies the source's space.
#[test]
fn a_selection_across_a_soft_wrap_keeps_the_source_space() {
    let body = "alpha beta gamma delta epsilon\n";
    let mut app = App::new("m", "s");
    assistant(&mut app, body);
    app.set_transcript_width(20);
    let modes = modes(ReasoningMode::Expanded);
    let blocks = blocks::parse_blocks(body);
    let lines = blocks::layout(&app.entries[0], &blocks, modes, 20);
    assert!(lines.len() > 1, "the paragraph wraps: {lines:?}");
    // From 'b' in "beta" to 'g' in "gamma" (display positions 6..11).
    let (sl, sc) = cell_of(&lines, 6).expect("start cell");
    let (el, ec) = cell_of(&lines, 11).expect("end cell");
    let anchor = select::locate(&app.entries, modes, 20, sl, sc + 2).expect("anchor");
    // The head is the cell after 'g': the copy then covers `beta gamma`.
    let head = select::locate(&app.entries, modes, 20, el, ec + 1 + 2).expect("head");
    assert_eq!(anchor.offset, 6, "'b' is byte 6 in the source");
    assert_eq!(head.offset, 12, "the 'a' after 'g' is byte 12");
    let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
    assert_eq!(
        copied, "beta g",
        "the source's space survives the soft wrap"
    );
}

/// The merge's integration point (s4's per-entry expansion × R2's Markdown): a `Ctrl-F` hit
/// inside a Markdown paragraph still lands on that paragraph when a folded reasoning channel
/// sits above it. The search measures the channel **fully expanded** (ADR 0028 R6), the jump
/// measures it folded (the display) - the two geometries differ, so the hit's absolute line has
/// to be recomputed against the display. This drives `App::find_jump` itself, so a jump that
/// measured the entries above expanded would land two lines off. The selection then copies the
/// source the paragraph covers.
#[test]
fn a_find_jump_lands_on_the_markdown_paragraph_under_a_folded_entry() {
    let mut app = App::new("m", "s");
    // A reasoning channel above: twenty lines expanded, one line folded.
    let thinking: String = (1..=20).map(|i| format!("thinking {i}\n")).collect();
    app.entries
        .push(radian_tui::app::Entry::new(EntryKind::Reasoning, &thinking));
    assistant(&mut app, "A **bold** paragraph here\n");
    let width = 40;
    let _ = render(&mut app, width as u16, 12);
    let modes = app.display_modes();
    let search = blocks::search_modes(modes);
    let folded = render::display_lines(&app.entries[0], modes, width).len();
    let expanded = render::display_lines(&app.entries[0], search, width).len();
    assert!(folded < expanded, "folded {folded} vs expanded {expanded}");
    app.on_key(ctrl('f'));
    for c in "bold".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let find = app.find_state().expect("the find box is open");
    assert_eq!(find.matches.len(), 1, "the rendered paragraph is searched");
    let m = find.matches[0];
    assert_eq!(m.entry, 1);
    // `current_line` is the **search** geometry (the channel expanded); the jump recomputes
    // the display line with the channel folded.
    assert_eq!(find.current_line(), Some(expanded + m.line));
    assert!(!app.is_following(), "a jump closes follow");
    let hit_line = folded + m.line;
    // The jump scrolled the paragraph into view: a jump that kept the expanded line would put
    // it twenty lines down, off a twelve-row screen.
    let terminal = render(&mut app, width as u16, 12);
    assert!(
        screen(&terminal).contains("bold"),
        "the paragraph is on screen after the jump:\n{}",
        screen(&terminal)
    );
    let lines = render::display_lines(&app.entries[1], modes, width);
    assert!(
        lines[m.line].contains("bold"),
        "the match is the paragraph line: {:?}",
        lines[m.line]
    );
    // The selection at that display line copies the paragraph's source (styles included).
    let anchor = select::locate(&app.entries, modes, width, hit_line, 0).expect("anchor");
    let head = select::locate(
        &app.entries,
        modes,
        width,
        hit_line,
        lines[m.line].chars().count() + 2,
    )
    .expect("head");
    let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
    assert!(
        copied.contains("**bold**"),
        "the copy is the paragraph's source: {copied:?}"
    );
}
