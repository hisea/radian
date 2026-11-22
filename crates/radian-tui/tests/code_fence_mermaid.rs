//! A `mermaid` code block is an ordinary code block (ADR 0035): the renderer was removed, so
//! the source shows inside the open line box, closed or not.

use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const FLOW: &str = "flowchart LR; A[Start] -- go --> B[End]";

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
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

#[test]
fn a_closed_mermaid_block_shows_its_source_in_a_code_frame() {
    let mut app = App::new("m", "s");
    app.session.entries.push(Entry::new(
        EntryKind::Assistant,
        format!("```mermaid\n{FLOW}\n```\n"),
    ));
    let text = screen(&mut app, 80, 20);
    assert!(
        text.contains("╭─ mermaid"),
        "the frame titles the block:\n{text}"
    );
    assert!(text.contains(FLOW), "the source shows as code:\n{text}");
    assert!(
        text.contains("╰"),
        "the closed block has its bottom line:\n{text}"
    );
}

#[test]
fn an_unclosed_mermaid_block_shows_its_source() {
    let mut app = App::new("m", "s");
    app.session.entries.push(Entry::new(
        EntryKind::Assistant,
        format!("```mermaid\n{FLOW}\n"),
    ));
    let text = screen(&mut app, 80, 20);
    assert!(text.contains(FLOW), "the source shows:\n{text}");
}
