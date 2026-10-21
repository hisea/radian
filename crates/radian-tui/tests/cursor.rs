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
    // The `lines` style (the `box` style is exercised below).
    app.input_style = radian_tui::render::InputStyle::Lines;
    app.input.set_text("你好");
    let terminal = render(&mut app, 20, 8);
    let cursor = terminal.backend().cursor_position();
    // The input area has a `❯ ` prompt (two cells), so the text starts at x = 2; two 2-cell
    // characters put the cursor at column 4.
    assert_eq!(cursor.x, 6, "prompt + two 2-cell characters");
    // The area is three rows tall at the bottom (a rule, one text row, a rule).
    assert_eq!(cursor.y, 5);
}

#[test]
fn the_cursor_follows_soft_wrap() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = radian_tui::render::InputStyle::Lines;
    app.input.set_text("abcdefghij");
    let terminal = render(&mut app, 10, 8);
    let cursor = terminal.backend().cursor_position();
    // The 8-wide text area wraps after `abcdefgh`; the cursor is on the next row at column 2.
    assert_eq!(cursor.x, 4, "prompt + column 2");
    assert_eq!(cursor.y, 5);
}

#[test]
fn a_multiline_input_grows_the_box_and_a_long_one_scrolls_to_the_cursor() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = InputStyle::Lines;
    app.input.set_text("a\nb\nc");
    let terminal = render(&mut app, 20, 12);
    let cursor = terminal.backend().cursor_position();
    // Three text rows + two rules = 5; the cursor is on the last text row.
    assert_eq!(cursor.y, 12 - 1 - 5 + 1 + 2);

    // More than ten rows: the box caps and follows the cursor instead of growing.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = InputStyle::Lines;
    app.input.set_text(&"x\n".repeat(20));
    let terminal = render(&mut app, 20, 30);
    let cursor = terminal.backend().cursor_position();
    // The area is 12 tall (10 text rows + 2 rules), at the bottom of the screen.
    assert_eq!(cursor.y, 30 - 1 - 12 + 1 + 9);
}

/// The `box` style's cursor: the `│ ` body rail (two cells) plus the `❯ ` prompt's **two
/// display cells** (the prompt's byte length used to shift it two
/// cells right, because `❯` is three bytes).
#[test]
fn the_box_cursor_counts_the_prompts_display_width() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = InputStyle::Box;
    // Empty: the body rail (2) + the prompt (2 cells).
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 4, "empty");
    // One ASCII character: right after it.
    app.input.set_text("f");
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 5, "ASCII");
    // A CJK character is two cells.
    app.input.set_text("你");
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 6, "CJK");
    // The text sits on the row below the frame's top rule (the bottom tail is behind it).
    assert_eq!(
        terminal.backend().cursor_position().y,
        5,
        "the text row inside the frame"
    );
}

/// The `lines` style's cursor for the same cases (its prompt is the same two cells, without a
/// border).
#[test]
fn the_lines_cursor_counts_the_prompts_display_width() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = InputStyle::Lines;
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 2, "empty");
    app.input.set_text("f");
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 3, "ASCII");
    app.input.set_text("你");
    let terminal = render(&mut app, 20, 8);
    assert_eq!(terminal.backend().cursor_position().x, 4, "CJK");
}

/// A continuation line aligns with the first line's text in **both** styles: the indent is the
/// prompt's display width, not its byte length.
#[test]
fn a_continuation_line_aligns_with_the_first_line() {
    use radian_tui::render::InputStyle;
    for style in [InputStyle::Lines, InputStyle::Box] {
        let mut app = App::new("m", "s");
        classic_footer(&mut app);
        app.input_style = style;
        app.input.set_text("a\nb");
        let terminal = render(&mut app, 20, 10);
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..10)
            .map(|y| {
                (0..20)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|c| c.symbol())
                            .unwrap_or(" ")
                            .to_string()
                    })
                    .collect()
            })
            .collect();
        let first = rows
            .iter()
            .position(|row| row.contains("❯ a"))
            .unwrap_or_else(|| panic!("{style:?}: no first line in {rows:?}"));
        let continuation = &rows[first + 1];
        let text_col = first_text_column(&rows[first]);
        let continuation_col = first_text_column(continuation);
        assert_eq!(
            continuation_col, text_col,
            "{style:?}: `{}` must start under `a` in `{}`",
            continuation, rows[first]
        );
        assert!(continuation.contains('b'), "{style:?}: {continuation:?}");
    }
}

/// The display column of the first non-space character after the prompt/border.
fn first_text_column(row: &str) -> usize {
    row.chars().position(|c| c.is_alphanumeric()).unwrap_or(0)
}
