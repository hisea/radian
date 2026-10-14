//! The transcript's Mermaid blocks (ADR 0028 R8): a closed ```mermaid block is drawn as a
//! Unicode diagram when `tui.display.mermaid` is on, and falls back to the source (with a dim
//! reason line) whenever the vendor cannot render it. `TestBackend` frames, so the three
//! diagram roles' colors are asserted on real cells.

use radian_tui::app::{App, EntryKind};
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A minimal flowchart the vendor renders well inside 80 columns.
const FLOW: &str = "flowchart LR; A[Start] -- go --> B[End]";

fn assistant(app: &mut App, text: &str) {
    app.entries
        .push(radian_tui::app::Entry::new(EntryKind::Assistant, text));
}

fn fenced(source: &str, closed: bool) -> String {
    let mut text = format!("```mermaid\n{source}\n");
    if closed {
        text.push_str("```\n");
    }
    text
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

/// The style of the first cell whose symbol starts with `needle`.
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

/// A closed flowchart on a wide screen draws box drawing and no fence.
#[test]
fn a_closed_flowchart_is_drawn_and_the_fence_is_not_shown() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    let text = screen(&render(&mut app, 80, 12));
    assert!(
        text.contains('┌') || text.contains('└') || text.contains('│'),
        "the diagram's box drawing is on screen:\n{text}"
    );
    assert!(text.contains("Start"), "a node label is drawn:\n{text}");
    assert!(
        !text.contains("```"),
        "the fences are markup and do not show:\n{text}"
    );
    assert!(
        !text.contains("flowchart LR"),
        "the source does not show when the diagram renders:\n{text}"
    );
}

/// The same diagram at a width it cannot fit falls back to the source plus the dim reason.
#[test]
fn a_diagram_that_does_not_fit_keeps_the_source_and_names_the_reason() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    // Narrow enough that the vendor reports `TooWide`.
    let text = screen(&render(&mut app, 16, 12));
    assert!(
        text.contains("mermaid:"),
        "the reason line is shown:\n{text}"
    );
    assert!(
        text.contains("flowchart") || text.contains("A[Start]"),
        "the source is kept:\n{text}"
    );
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "no half diagram is drawn:\n{text}"
    );
}

/// An unclosed block (still streaming) shows the source; it renders only once closed.
#[test]
fn an_unclosed_block_shows_the_source() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, false));
    let text = screen(&render(&mut app, 80, 12));
    assert!(text.contains("flowchart LR"), "the source shows:\n{text}");
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "nothing is drawn before the fence closes:\n{text}"
    );
}

/// `tui.display.mermaid = false` renders every code block as source.
#[test]
fn the_config_flag_disables_the_diagram() {
    let mut app = App::new("m", "s");
    app.mermaid_enabled = false;
    assistant(&mut app, &fenced(FLOW, true));
    let text = screen(&render(&mut app, 80, 12));
    assert!(text.contains("flowchart LR"), "the source shows:\n{text}");
    assert!(
        !text.contains('┌') && !text.contains('└'),
        "nothing is drawn when the flag is off:\n{text}"
    );
}

/// The three roles take their colors from the theme: the box drawing is the node role, the
/// connector and its label the edge role, and a node's label text the plain-text role.
#[test]
fn the_diagram_roles_come_from_the_theme() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    let terminal = render(&mut app, 80, 12);
    let node = style_of(&terminal, "┌").expect("a box-drawing char");
    let edge = style_of(&terminal, "go").expect("an edge label");
    let text = style_of(&terminal, "Start").expect("a node label");
    assert_eq!(node.fg, app.theme.mermaid_node.fg, "box drawing is a node");
    assert_eq!(
        edge.fg, app.theme.mermaid_edge.fg,
        "the connector is an edge"
    );
    assert_eq!(
        text.fg, app.theme.mermaid_text.fg,
        "the label is plain text"
    );
    assert_ne!(node.fg, edge.fg, "nodes and edges differ");
}

/// A width change re-renders: the counted lines equal the laid-out (drawn) body lines at both
/// widths, and the two widths lay out differently.
#[test]
fn a_width_change_recounts_and_redraws() {
    let mut app = App::new("m", "s");
    assistant(&mut app, &fenced(FLOW, true));
    let entry = &app.entries[0];
    let blocks = blocks::parse_blocks(entry.text.as_str());
    for width in [80usize, 16] {
        let count = blocks::line_count(entry, ReasoningMode::Expanded, width, true);
        let laid_out = blocks::layout(entry, &blocks, ReasoningMode::Expanded, width, true).len();
        assert_eq!(count, laid_out, "width {width}: count == laid out");
    }
    // Wide: the diagram renders (box drawing); narrow: it falls back to the source.
    let wide = screen(&render(&mut app, 80, 12));
    assert!(wide.contains('┌'), "wide draws the diagram:\n{wide}");
    let narrow = screen(&render(&mut app, 16, 12));
    assert!(
        narrow.contains("mermaid:"),
        "narrow names the reason:\n{narrow}"
    );
    assert!(!narrow.contains('┌'), "narrow draws no diagram:\n{narrow}");
}
