//! Code-block syntax highlighting (ADR 0028 R2, decision 2): colors come from the
//! theme's nine `syntax*` roles, only closed blocks with a known language highlight, and the
//! text is unchanged (copy and search see the same characters). `TestBackend` frames, so the
//! colors are asserted on real cells.

use std::path::Path;

use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::render;
use radian_tui::theme::{self, Theme};
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Style;

fn modes(highlight: bool) -> DisplayModes {
    DisplayModes {
        reasoning: ReasoningMode::Expanded,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight,
    }
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

/// The style of the first cell of the line that first contains `needle`.
fn style_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<Style> {
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

fn assistant(app: &mut App, text: &str) {
    app.entries.push(Entry::new(EntryKind::Assistant, text));
}

const RUST: &str = "```rust\nfn main() {\n    let s = \"hi\";\n    // note\n}\n```\n";

/// The acceptance: a Rust block's keyword, string and comment take the theme's
/// `syntax_keyword` / `syntax_string` / `syntax_comment` colors.
#[test]
fn rust_keyword_string_and_comment_take_their_theme_roles() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(&mut app, RUST);
    let terminal = render(&mut app, 60, 20);
    assert_eq!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg,
        "`fn` must be the keyword color"
    );
    assert_eq!(
        style_of(&terminal, "hi").and_then(|s| s.fg),
        theme.syntax_string.fg,
        "`hi` must be the string color"
    );
    assert_eq!(
        style_of(&terminal, "note").and_then(|s| s.fg),
        theme.syntax_comment.fg,
        "`note` must be the comment color"
    );
    assert_eq!(
        style_of(&terminal, "main").and_then(|s| s.fg),
        theme.syntax_function.fg,
        "`main` must be the function color"
    );
}

/// A kanagawa-wave fixture's `syntaxKeyword` reaches the cells, i.e. a theme file's colors are
/// respected (not syntect's own palette).
#[test]
fn a_theme_files_syntax_role_reaches_the_cells() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/themes");
    let (theme, warnings) = theme::load(Some("kanagawa-wave"), false, true, Some(&dir), &[]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let keyword = theme
        .syntax_keyword
        .fg
        .expect("the fixture sets syntaxKeyword");
    let string = theme
        .syntax_string
        .fg
        .expect("the fixture sets syntaxString");
    assert_ne!(keyword, string, "the fixture's roles differ");

    let mut app = App::new("m", "s");
    app.theme = theme;
    assistant(&mut app, RUST);
    let terminal = render(&mut app, 60, 20);
    assert_eq!(style_of(&terminal, "fn").and_then(|s| s.fg), Some(keyword));
    assert_eq!(style_of(&terminal, "hi").and_then(|s| s.fg), Some(string));
}

/// An unclosed fence (a streaming block) is plain until the closing fence arrives, matching
/// R8's "only a closed block is rendered".
#[test]
fn an_unclosed_block_is_not_highlighted() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(&mut app, "```rust\nfn main() {\n");
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(text.contains("fn main"), "{text}");
    assert_ne!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg,
        "an open block must stay plain: {text}"
    );
}

/// A language outside the table stays plain even when the grammar exists.
#[test]
fn an_unknown_language_is_not_highlighted() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(&mut app, "```nonesuch\nfn main() {\n```\n");
    let terminal = render(&mut app, 60, 10);
    assert_ne!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg
    );
    // No info string at all is the same.
    let mut app = App::new("m", "s");
    assistant(&mut app, "```\nfn main() {\n```\n");
    let terminal = render(&mut app, 60, 10);
    assert_ne!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg
    );
}

/// `tui.display.highlight = false` turns the whole feature off.
#[test]
fn highlight_false_disables_the_colors() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    app.highlight_enabled = false;
    assistant(&mut app, RUST);
    let terminal = render(&mut app, 60, 20);
    assert_ne!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg
    );
}

/// A truncated line keeps the colors of the characters that survive, and the ellipsis is
/// present.
#[test]
fn a_truncated_line_keeps_the_color_before_the_ellipsis() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    // The string is wide enough to be cut at width 24.
    assistant(
        &mut app,
        "```rust\nlet value = \"a-long-string-that-is-cut\";\n```\n",
    );
    let terminal = render(&mut app, 24, 10);
    let text = screen(&terminal);
    assert!(
        text.contains('…'),
        "the line must be cut with an ellipsis: {text}"
    );
    // `a` is the first string char; it must still be string-colored.
    assert_eq!(
        style_of(&terminal, "a-long").and_then(|s| s.fg),
        theme.syntax_string.fg,
        "{text}"
    );
}

/// The text is unchanged by highlighting: layout and the search/reference path produce the
/// same characters with and without it.
#[test]
fn highlighting_does_not_change_the_text() {
    let mut entry = Entry::new(EntryKind::Assistant, RUST);
    let mut plain = Entry::new(EntryKind::Assistant, RUST);
    let blocks = blocks::blocks_for(&mut entry, ReasoningMode::Expanded);
    let plain_blocks = blocks::blocks_for(&mut plain, ReasoningMode::Expanded);
    let with: Vec<String> = blocks::layout(&entry, &blocks, modes(true), 40)
        .into_iter()
        .map(|line| line.text)
        .collect();
    let without: Vec<String> = blocks::layout(&plain, &plain_blocks, modes(false), 40)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(with, without, "highlighting must not change the drawn text");
    // And the search/reference path (text only) agrees.
    let searched = blocks::body_text_lines(&entry, modes(true), 40);
    let searched_plain = blocks::body_text_lines(&plain, modes(false), 40);
    assert_eq!(searched, searched_plain);
}

/// The cache key carries the text length: appending a line re-highlights, so the new line is
/// colored instead of reusing the shorter block's result.
#[test]
fn appending_a_line_rehighlights_it() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(&mut app, "```rust\nfn a() {}\n```\n");
    let _ = render(&mut app, 60, 20);
    app.entries[0].text = "```rust\nfn a() {}\nfn b() {}\n```\n".to_owned();
    let terminal = render(&mut app, 60, 20);
    assert_eq!(
        style_of(&terminal, "fn b").and_then(|s| s.fg),
        theme.syntax_keyword.fg,
        "the longer block must re-highlight: {}",
        screen(&terminal)
    );
}
