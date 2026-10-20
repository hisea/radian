//! Diff bodies (ADR 0028): the synthesized line-number gutter, the sign,
//! the syntax highlight, the structure rows and the copy rule, on `TestBackend`.

use agent_core::{ContentPart, Message, ToolCall, ToolResult};
use radian_tui::app::{App, EntryKind};
use radian_tui::render;
use radian_tui::select::{Selection, locate, locate_hit, logical_text};
use radian_tui::tools::ToolFold;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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
            let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            out.push_str(symbol);
        }
        out.push('\n');
    }
    out
}

/// Finds the first cell whose symbol starts with `needle`, and returns its style.
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
            let x = area.left() + unicode_width::UnicodeWidthStr::width(&line[..col]) as u16;
            return buffer.cell((x, y)).map(|c| c.style());
        }
    }
    None
}

fn assistant_with(calls: Vec<ToolCall>) -> Message {
    Message::Assistant {
        content: Vec::new(),
        tool_calls: calls,
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
        reasoning: Vec::new(),
    }
}

/// A tool call plus its result, as the transcript gets them.
fn card(
    app: &mut App,
    name: &str,
    arguments: serde_json::Value,
    text: &str,
    structured: serde_json::Value,
) {
    app.push_message(&assistant_with(vec![ToolCall {
        id: "c1".into(),
        name: name.into(),
        arguments,
    }]));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text(text)],
            structured: Some(structured),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
}

/// A `patch` result with two replaced ranges (`hunks`, ADR 0028): every
/// row draws its own file's line number — `-` rows the old file's, `+` rows the new one's — and
/// the second range's rows sit a line higher in the new file, because the first range replaced
/// two lines with three.
#[test]
fn a_patch_body_numbers_its_rows_once_per_hunk() {
    let mut app = App::new("m", "s");
    card(
        &mut app,
        "patch",
        serde_json::json!({
            "path": "src/main.rs",
            "old": "let a = 1;\nlet b = 2;\n",
            "new": "let a = 1;\nlet b = 2;\nlet c = 3;\n",
            "replace_all": true,
        }),
        "replaced 2 occurrences in src/main.rs (matched by exact)",
        serde_json::json!({
            "path": "src/main.rs",
            "replaced": 2,
            "matched": "exact",
            "hunks": [
                {"old_start": 10, "old_lines": 2, "new_start": 10, "new_lines": 3},
                {"old_start": 40, "old_lines": 2, "new_start": 41, "new_lines": 3},
            ],
        }),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    // Row by row: the header, then the removed rows numbered in the old file and the added
    // rows in the new one.
    assert!(text.contains("@@ -10,2 +10,3 @@"), "{text}");
    assert!(text.contains("  10 - let a = 1;"), "{text}");
    assert!(text.contains("  11 - let b = 2;"), "{text}");
    assert!(text.contains("  10 + let a = 1;"), "{text}");
    assert!(text.contains("  11 + let b = 2;"), "{text}");
    assert!(text.contains("  12 + let c = 3;"), "{text}");
    // The second range: its old rows keep the old numbers, its new rows start where the first
    // range's delta put them (41, not 40).
    assert!(text.contains("@@ -40,2 +41,3 @@"), "{text}");
    assert!(text.contains("  40 - let a = 1;"), "{text}");
    assert!(text.contains("  41 - let b = 2;"), "{text}");
    assert!(text.contains("  41 + let a = 1;"), "{text}");
    assert!(text.contains("  43 + let c = 3;"), "{text}");
}

/// A unified diff text: the numbers come from the text's own `@@` headers, and the row's
/// own side picks which file's counter moves.
#[test]
fn a_patch_body_numbers_unified_text_from_its_hunk_headers() {
    let mut app = App::new("m", "s");
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                index 1..2 100644\n\
                --- a/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -10,2 +10,2 @@ fn main() {\n\
                \x20let a = 1;\n\
                -let b = 2;\n\
                +let b = 3;\n";
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "src/main.rs"}),
        diff,
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("@@ -10,2 +10,2 @@"), "{text}");
    assert!(text.contains("  10   let a = 1;"), "{text}");
    assert!(text.contains("  11 - let b = 2;"), "{text}");
    assert!(text.contains("  11 + let b = 3;"), "{text}");
    // A file header carries no number at all: it is structure.
    assert!(!text.contains("  10 diff --git"), "{text}");
}

/// The colors: the number is the weak `diff_line_number`, the sign its own side's role, the
/// code is highlighted by the file's suffix, and `@@`/file headers are the frame's weak role —
/// never the accent (ADR 0028, ADR 0029).
#[test]
fn a_diff_row_colors_its_gutter_sign_and_code() {
    let mut app = App::new("m", "s");
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -1,1 +1,2 @@\n\
                -let a = 1;\n\
                +let a = 2;\n\
                +let b = 3;\n";
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "src/main.rs"}),
        diff,
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let terminal = render(&mut app, 80, 24);
    let theme = app.theme;

    let number = style_of(&terminal, "  1 -").expect("the removed row's gutter");
    assert_eq!(
        number.fg, theme.diff_line_number.fg,
        "the line number is the weak diff line-number role"
    );
    assert_eq!(
        style_of(&terminal, "- let a = 1;").expect("the sign").fg,
        theme.diff_removed.fg,
        "the `-` sign carries the removed role"
    );
    assert_eq!(
        style_of(&terminal, "+ let a = 2;").expect("the sign").fg,
        theme.diff_added.fg,
        "the `+` sign carries the added role"
    );
    // A `.rs` diff highlights: the code's `let` is a keyword, and the code is not the sign's
    // colour (highlighting changes the colours, never the text).
    assert_eq!(
        style_of(&terminal, "let a = 1;").expect("the code").fg,
        theme.syntax_keyword.fg,
        "a `.rs` diff's code is highlighted through the code-block table"
    );
    // `@@` and the file headers are structure: the weak frame role, never the accent.
    let hunk = style_of(&terminal, "@@ -1,1").expect("the hunk header");
    assert_eq!(hunk.fg, theme.frame.fg, "`@@` is the weak frame role");
    assert_ne!(hunk.fg, theme.notice_label.fg, "`@@` is not the accent");
    let header = style_of(&terminal, "diff --git").expect("the file header");
    assert_eq!(header.fg, theme.frame.fg, "file headers are structure too");
    // No row of the diff carries a background **of its own**:
    // an added row, a removed row and a structure row all sit on the card's own band, which is
    // the revision-4 card's (slice ① removes that band, not this slice).
    let added = style_of(&terminal, "+ let a = 2;").expect("the added sign");
    assert_eq!(
        added.bg, header.bg,
        "a `+` row paints no background of its own"
    );
    assert_eq!(
        number.bg, header.bg,
        "the gutter paints no background of its own"
    );
}

/// A diff whose text carries no `@@` header (an MCP tool's text, an older journal entry) keeps
/// today's numberless form: the sign column, no gutter.
#[test]
fn a_diff_without_start_lines_draws_no_gutter() {
    let mut app = App::new("m", "s");
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "notes.txt"}),
        "-gone\n+added\n same\n",
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let terminal = render(&mut app, 80, 20);
    let text = screen(&terminal);
    assert!(text.contains("-gone"), "{text}");
    assert!(text.contains("+added"), "{text}");
    assert!(text.contains(" same"), "{text}");
    // Nothing that looks like a 4-cell number plus a space before the sign.
    assert!(!text.contains("   0 -"), "{text}");
    assert!(!text.contains("   1 -"), "{text}");
    let theme = app.theme;
    assert_eq!(
        style_of(&terminal, "-gone").expect("the sign").fg,
        theme.diff_removed.fg
    );
}

/// The copy rule, written down: a diff row's text **is** the entry's text (the sign and the
/// code, exactly as before this change), and the synthesized gutter has no source, so a
/// selection can never copy a line number (ADR 0028's acceptance). This is what the
/// body copied before the gutter existed: the row as the diff text carries it.
#[test]
fn a_diff_row_copies_its_own_text_without_the_gutter() {
    let mut app = App::new("m", "s");
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -10,1 +10,1 @@\n\
                -let a = 1;\n\
                +let a = 2;\n";
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "src/main.rs"}),
        diff,
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    app.set_transcript_width(80);
    let modes = app.display_modes();
    let width = 80usize;
    // The result entry's own drawing, exactly as the selection mapping derives it.
    let entries = &app.session.entries;
    let result_index = entries
        .iter()
        .position(|entry| entry.kind == EntryKind::ToolResult)
        .expect("the result entry");
    let entry = &entries[result_index];
    let body = radian_tui::blocks::layout(entry, &[], modes, width);
    let row = body
        .iter()
        .position(|line| line.text.ends_with("- let a = 1;"))
        .expect("the removed row is drawn");
    assert_eq!(body[row].text, "  10 - let a = 1;");
    let text = entry.text.clone();
    let start = text.find("-let a = 1;").expect("the row's text");
    // The gutter's five cells are synthesized; the sign and the code map the entry's own bytes
    // (the space the row draws between them is decoration, and carries no source either).
    assert!(
        body[row].chars[..5].iter().all(Option::is_none),
        "the number carries no source: {:?}",
        body[row].chars
    );
    let mapped: Vec<usize> = body[row].chars.iter().flatten().copied().collect();
    assert_eq!(
        mapped,
        (start..start + "-let a = 1;".len()).collect::<Vec<_>>(),
        "the drawn cells map the row's own bytes: {:?}",
        body[row].chars
    );
    // A selection over the row, from the transcript's own cells: the lines of the entries
    // before it, their gaps, and the row's own line inside this entry (what `select::locate`
    // walks).
    let line = entries[..result_index]
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            radian_tui::blocks::gap_at(entries, index)
                + render::display_lines(entry, modes, width).len()
        })
        .sum::<usize>()
        + radian_tui::blocks::gap_at(entries, result_index)
        + row;
    // A press on the gutter's first cell anchors at the sign: a number is not selectable, and
    // the five gutter cells are synthesized.
    let hit = locate_hit(entries, modes, width, line, 0).expect("a hit");
    assert_eq!(hit.anchor.offset, start, "the gutter anchors at the sign");
    assert!(!hit.on_char, "a synthesized cell is not a char");
    let anchor = locate(entries, modes, width, line, 0).expect("the row's start");
    assert_eq!(anchor.offset, start);
    let head = locate(entries, modes, width, line, 200).expect("the row's end");
    let copied = logical_text(entries, &Selection { anchor, head });
    assert_eq!(
        copied, "-let a = 1;",
        "the copy is the row's own text: no gutter, the sign as the diff carries it"
    );
}

/// `write` (ADR 0028): a whole-file write has no hunk, so the entry text
/// is the file's content with **no** sign and every row is a numbered addition. The number and
/// the synthesized `+` are drawn but carry no source, so a copy of the body is the content
/// alone — no gutter, no sign.
#[test]
fn a_write_body_numbers_the_content_and_copies_it_without_the_gutter() {
    let mut app = App::new("m", "s");
    card(
        &mut app,
        "write",
        serde_json::json!({"path": "src/a.rs", "content": "let a = 1;\nlet b = 2;"}),
        "wrote 20 bytes to src/a.rs",
        serde_json::json!({"path": "src/a.rs", "bytes_written": 20}),
    );
    app.tool_display.fold = ToolFold::Expanded;
    app.set_transcript_width(80);
    let modes = app.display_modes();
    let width = 80usize;
    let entries = &app.session.entries;
    let result_index = entries
        .iter()
        .position(|entry| entry.kind == EntryKind::ToolResult)
        .expect("the result entry");
    let entry = &entries[result_index];
    let body = radian_tui::blocks::layout(entry, &[], modes, width);
    let row = body
        .iter()
        .position(|line| line.text.ends_with("+ let a = 1;"))
        .expect("the first added row is drawn");
    assert_eq!(body[row].text, "   1 + let a = 1;");
    // The four-cell number, its space, the sign and the gap after it are synthesized: seven
    // cells with no source. Only the code maps the entry's own bytes.
    assert!(
        body[row].chars[..7].iter().all(Option::is_none),
        "the number and the sign carry no source: {:?}",
        body[row].chars
    );
    let start = entry.text.find("let a = 1;").expect("the code's text");
    let mapped: Vec<usize> = body[row].chars.iter().flatten().copied().collect();
    assert_eq!(
        mapped,
        (start..start + "let a = 1;".len()).collect::<Vec<_>>(),
        "the drawn cells map the code's own bytes: {:?}",
        body[row].chars
    );
    // A selection over the whole entry copies the content, never the numbering or the sign.
    let base = entries[..result_index]
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            radian_tui::blocks::gap_at(entries, index)
                + render::display_lines(entry, modes, width).len()
        })
        .sum::<usize>()
        + radian_tui::blocks::gap_at(entries, result_index);
    let anchor = locate(entries, modes, width, base, 0).expect("the first row's start");
    let head =
        locate(entries, modes, width, base + body.len() - 1, 200).expect("the last row's end");
    let copied = logical_text(entries, &Selection { anchor, head });
    assert_eq!(
        copied, "let a = 1;\nlet b = 2;",
        "the copy is the content, with no gutter and no sign"
    );
}

/// The line count a card reports equals the lines it draws, with numbered diff rows — at two
/// widths, including one where a row has to be cut (ADR 0028 R1's virtualization: the count
/// and the drawing derive from the same plan).
#[test]
fn a_diff_body_line_count_matches_its_layout() {
    let mut app = App::new("m", "s");
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -1,2 +1,2 @@\n\
                -let a = 1; let b = 2; let c = 3; let d = 4; let e = 5; let f = 6; let g = 7;\n\
                +let a = 2;\n";
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "src/main.rs"}),
        diff,
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let modes = app.display_modes();
    let entry = app
        .session
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::ToolResult)
        .expect("the result entry");
    for width in [80usize, 30] {
        let count = radian_tui::blocks::line_count(entry, modes, width);
        let laid_out = radian_tui::blocks::layout(entry, &[], modes, width).len();
        assert_eq!(count, laid_out, "width {width}");
    }
}

/// A diff row never wraps (R9): a row wider than the body is truncated with `…` and stays one
/// transcript line, and its cells still cover the row's source (the cut tail copies).
#[test]
fn a_long_diff_row_truncates_instead_of_wrapping() {
    let mut app = App::new("m", "s");
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -1,1 +1,1 @@\n\
                -let a = 1;\n\
                +let a = 2; let b = 3; let c = 4; let d = 5; let e = 6; let f = 7; let g = 8;\n";
    card(
        &mut app,
        "patch",
        serde_json::json!({"path": "src/main.rs"}),
        diff,
        serde_json::json!(null),
    );
    app.tool_display.fold = ToolFold::Expanded;
    let modes = app.display_modes();
    let entry = app
        .session
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::ToolResult)
        .expect("the result entry");
    let body = radian_tui::blocks::layout(entry, &[], modes, 40);
    let rows: Vec<&radian_tui::blocks::BodyLine> = body
        .iter()
        .filter(|line| line.text.contains("let a = 2;"))
        .collect();
    assert_eq!(rows.len(), 1, "the row is one line, not a wrapped pair");
    assert!(rows[0].text.ends_with('…'), "{:?}", rows[0].text);
    // The tool block's marker column is two cells (ADR 0028), so at
    // width 40 the body is 38 cells: the row keeps 37 of them and spends the last on the `…`.
    assert_eq!(rows[0].text.chars().count(), 38, "{:?}", rows[0].text);
    // The last cell maps to a source byte, so the copy of the row still reaches its text.
    assert!(
        rows[0].chars.last().copied().flatten().is_some(),
        "the ellipsis keeps the last cell's source: {:?}",
        rows[0].chars
    );
}
