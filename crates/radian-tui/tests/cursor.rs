//! The input cursor's cell under wide characters and soft wrap (ADR 0009 Phase 4), asserted on
//! a `TestBackend` frame.

use radian_tui::App;

/// Puts a test on the pre-ADR one-line status bar (`preset = classic`; ADR 0027 C2). The
/// default footer is the two-row `pi` layout now; a geometry test keeps the layout it was
/// written for.
fn classic_footer(app: &mut App) {
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
}
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
    classic_footer(&mut app);
    app.input.set_text("你好");
    let terminal = render(&mut app, 20, 8);
    let cursor = terminal.backend().cursor_position();
    // The input area has a `> ` prompt, so the text starts at x = 2; two 2-cell characters put
    // the cursor at column 4.
    assert_eq!(cursor.x, 6, "prompt + two 2-cell characters");
    // The area is three rows tall at the bottom (a rule, one text row, a rule).
    assert_eq!(cursor.y, 5);
}

#[test]
fn the_cursor_follows_soft_wrap() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input.set_text("abcdefghij");
    let terminal = render(&mut app, 10, 8);
    let cursor = terminal.backend().cursor_position();
    // The 8-wide text area wraps after `abcdefgh`; the cursor is on the next row at column 2.
    assert_eq!(cursor.x, 4, "prompt + column 2");
    assert_eq!(cursor.y, 5);
}

#[test]
fn a_multiline_input_grows_the_box_and_a_long_one_scrolls_to_the_cursor() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input.set_text("a\nb\nc");
    let terminal = render(&mut app, 20, 12);
    let cursor = terminal.backend().cursor_position();
    // Three text rows + two rules = 5; the cursor is on the last text row.
    assert_eq!(cursor.y, 12 - 1 - 5 + 1 + 2);

    // More than ten rows: the box caps and follows the cursor instead of growing.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input.set_text(&"x\n".repeat(20));
    let terminal = render(&mut app, 20, 30);
    let cursor = terminal.backend().cursor_position();
    // The area is 12 tall (10 text rows + 2 rules), at the bottom of the screen.
    assert_eq!(cursor.y, 30 - 1 - 12 + 1 + 9);
}
