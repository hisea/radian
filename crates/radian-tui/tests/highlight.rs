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
        glyphs: radian_tui::footer::Glyphs::Unicode,
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

/// The vendored TypeScript and TOML grammars are loaded and highlight with the theme roles.
#[test]
fn typescript_and_toml_highlight_with_theme_roles() {
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "```typescript\nconst greeting: string = \"hi\"; // note\n```\n",
    );
    let terminal = render(&mut app, 60, 12);
    assert_eq!(
        style_of(&terminal, "const").and_then(|s| s.fg),
        theme.syntax_keyword.fg,
        "`const` must be the keyword color"
    );
    assert_eq!(
        style_of(&terminal, "hi").and_then(|s| s.fg),
        theme.syntax_string.fg,
        "`hi` must be the string color"
    );

    let mut app = App::new("m", "s");
    assistant(&mut app, "```toml\ntitle = \"demo\" # note\n```\n");
    let terminal = render(&mut app, 60, 12);
    assert_eq!(
        style_of(&terminal, "demo").and_then(|s| s.fg),
        theme.syntax_string.fg,
        "the TOML string must take the string color"
    );
    assert_eq!(
        style_of(&terminal, "note").and_then(|s| s.fg),
        theme.syntax_comment.fg,
        "the TOML comment must take the comment color"
    );

    // TSX is recognized too (its own grammar), and the alias set covers `ts`/`tsx`.
    for lang in ["ts", "typescript", "tsx", "toml"] {
        assert!(
            radian_tui::highlight::syntax_for(lang).is_some(),
            "`{lang}` must resolve to a vendored grammar"
        );
    }
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

/// A code block of `n` Rust lines, unique per call so a test never shares another test's
/// content-keyed cache entry.
fn rust_block(n: usize, tag: &str) -> String {
    let mut body = format!("```rust\n// {tag}\n");
    for i in 0..n {
        body.push_str(&format!("fn {tag}_{i}() -> i32 {{ {i} }}\n"));
    }
    body.push_str("```\n");
    body
}

/// H1: the measure pass never highlights. Highlighting is a pure function of the body and
/// language, and the line count does not need colors, so measuring every entry (the resize and
/// resume path) must not pay for it. The counter is the proof.
#[test]
fn the_measure_pass_never_highlights() {
    let theme = Theme::dark();
    let modes = modes(true);
    let mut entries: Vec<Entry> = (0..8)
        .map(|i| Entry::new(EntryKind::Assistant, rust_block(30, &format!("measure{i}"))))
        .collect();
    let before = radian_tui::highlight::highlight_calls();
    // Height 0: measure only, no visible line is laid out.
    let (total, lines) = radian_tui::render::visible_lines(&mut entries, modes, 80, 0, 0, &theme);
    assert!(total > 0 && lines.is_empty(), "measured {total}");
    assert_eq!(
        radian_tui::highlight::highlight_calls(),
        before,
        "the measure pass must not highlight"
    );
    // A visible window does highlight its block.
    let _ = radian_tui::render::visible_lines(&mut entries, modes, 80, 0, 20, &theme);
    assert!(
        radian_tui::highlight::highlight_calls() > before,
        "laying a visible code block must highlight it"
    );
}

/// H1: a resize reuses the highlight cache — the roles do not depend on the width, so a width
/// change must not re-highlight every block in the session.
#[test]
fn a_resize_reuses_the_highlight_cache() {
    let theme = Theme::dark();
    let modes_on = modes(true);
    let mut entries = vec![Entry::new(EntryKind::Assistant, rust_block(60, "resize"))];
    let _ = radian_tui::render::visible_lines(&mut entries, modes_on, 100, 0, 40, &theme);
    let after_first = radian_tui::highlight::highlight_calls();
    let _ = radian_tui::render::visible_lines(&mut entries, modes_on, 80, 0, 40, &theme);
    let _ = radian_tui::render::visible_lines(&mut entries, modes_on, 60, 0, 40, &theme);
    assert_eq!(
        radian_tui::highlight::highlight_calls(),
        after_first,
        "a resize must not re-highlight"
    );
    // With highlighting off it is not called at all (only the flag changes).
    let mut plain = vec![Entry::new(
        EntryKind::Assistant,
        rust_block(60, "resize-off"),
    )];
    let before = radian_tui::highlight::highlight_calls();
    let _ = radian_tui::render::visible_lines(&mut plain, modes(false), 100, 0, 40, &theme);
    assert_eq!(radian_tui::highlight::highlight_calls(), before);
}

/// H1 / R7: with many large code blocks, the first frame after a width change must not pay
/// for highlighting — the measure pass only counts, and the content cache survives the resize.
/// The comparison is **relative** (highlight on vs off on the same corpus): an absolute
/// millisecond gate is load-dependent, but a measure pass that highlighted every block would
/// be orders of magnitude slower than the same pass with highlighting off.
#[test]
fn resize_first_frame_does_not_pay_for_highlighting() {
    let theme = Theme::dark();
    let corpus = || -> Vec<Entry> {
        (0..60)
            .map(|i| Entry::new(EntryKind::Assistant, rust_block(500, &format!("big{i}"))))
            .collect()
    };
    let mut on = corpus();
    let mut off = corpus();
    // Warm each at one width, then time the resize frame.
    let _ = radian_tui::render::visible_lines(&mut on, modes(true), 100, 0, 40, &theme);
    let _ = radian_tui::render::visible_lines(&mut off, modes(false), 100, 0, 40, &theme);
    let started = std::time::Instant::now();
    let _ = radian_tui::render::visible_lines(&mut on, modes(true), 80, 0, 40, &theme);
    let with = started.elapsed();
    let started = std::time::Instant::now();
    let _ = radian_tui::render::visible_lines(&mut off, modes(false), 80, 0, 40, &theme);
    let without = started.elapsed();
    assert!(
        with <= without * 5 + std::time::Duration::from_millis(200),
        "highlighting must not run in the measure pass: on {with:?} vs off {without:?}"
    );
}

/// H2: a block over the size cap is not highlighted (the synchronous cost is bounded); one at
/// the cap is.
#[test]
fn a_block_over_the_cap_is_not_highlighted() {
    use radian_tui::highlight::{MAX_HIGHLIGHT_LINES, highlight_lines};
    let at = vec!["fn x() {}"; MAX_HIGHLIGHT_LINES];
    assert!(highlight_lines(&at, "rust").is_some());
    let over = vec!["fn x() {}"; MAX_HIGHLIGHT_LINES + 1];
    assert!(highlight_lines(&over, "rust").is_none());

    // And the render path leaves it plain.
    let theme = Theme::dark();
    let mut app = App::new("m", "s");
    assistant(&mut app, &rust_block(MAX_HIGHLIGHT_LINES + 1, "huge"));
    let terminal = render(&mut app, 40, 10);
    assert_ne!(
        style_of(&terminal, "fn").and_then(|s| s.fg),
        theme.syntax_keyword.fg,
        "an over-cap block must stay plain"
    );
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
