//! The input cursor's cell under wide characters and soft wrap (ADR 0009 Phase 4), asserted on
//! a `TestBackend` frame.

use radian_tui::App;
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

#[test]
fn the_cursor_cell_counts_display_width_not_chars() {
    let mut app = App::new("m", "s");
    app.input.set_text("你好");
    let terminal = render(&mut app, 20, 8);
    let cursor = terminal.backend().cursor_position();
    // The box has a border, so the inner cursor starts at x = 1; two 2-cell characters put it
    // at column 4.
    assert_eq!(cursor.x, 5, "border + two 2-cell characters");
    // The box is three rows tall at the bottom (a border, one text row, a border).
    assert_eq!(cursor.y, 5);
}

#[test]
fn the_cursor_follows_soft_wrap() {
    let mut app = App::new("m", "s");
    app.input.set_text("abcdefghij");
    let terminal = render(&mut app, 10, 8);
    let cursor = terminal.backend().cursor_position();
    // The 8-wide inner area wraps after `abcdefgh`; the cursor is on the next row at column 2.
    assert_eq!(cursor.x, 3, "border + column 2");
    assert_eq!(cursor.y, 5);
}

#[test]
fn a_multiline_input_grows_the_box_and_a_long_one_scrolls_to_the_cursor() {
    let mut app = App::new("m", "s");
    app.input.set_text("a\nb\nc");
    let terminal = render(&mut app, 20, 12);
    let cursor = terminal.backend().cursor_position();
    // Three text rows + two borders = 5; the cursor is on the last text row.
    assert_eq!(cursor.y, 12 - 1 - 5 + 1 + 2);

    // More than ten rows: the box caps and follows the cursor instead of growing.
    let mut app = App::new("m", "s");
    app.input.set_text(&"x\n".repeat(20));
    let terminal = render(&mut app, 20, 30);
    let cursor = terminal.backend().cursor_position();
    // The box is 12 tall (10 text rows + 2 borders), at the bottom of the screen.
    assert_eq!(cursor.y, 30 - 1 - 12 + 1 + 9);
}
