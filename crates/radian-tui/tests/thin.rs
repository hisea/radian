//! Slice ⑦: whole-screen snapshots of the thin redesign (ADR 0028), at 80 and 120
//! columns and under `dark`/`light`, with CJK, plus the mechanical rules the ADR's acceptance
//! names.
//!
//! A "snapshot" here is a plain-text frame (rows as strings) checked into the test; the styling
//! rules the text cannot show (no background, the role of a glyph) are asserted on the buffer
//! beside it. No `insta`. The transcript region is compared with the input area and the
//! `classic` footer cut off, so a golden is exactly the drawn content and a row that a later
//! change moves (e.g. the empty-turn fix) is a one-line edit.

use agent_core::{ContentPart, Message, ToolCall, ToolResult};
use radian_tui::app::{App, EntryKind, Input};
use radian_tui::render;
use radian_tui::theme::Theme;
use radian_tui::tools::{ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use serde_json::json;

const OK: char = '✓';
const FAILED: char = '×';
/// The `lines` input area (rule + row + rule) and the one-row `classic` footer.
const CHROME: u16 = 4;

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The transcript region with the blank filler rows dropped (the view follows the bottom, so a
/// short transcript leaves blanks above it).
fn transcript(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut rows = rows(&render(app, width, height));
    rows.truncate(usize::from(height - CHROME));
    while rows.last().is_some_and(|row| row.is_empty()) {
        rows.pop();
    }
    let first = rows
        .iter()
        .position(|row| !row.is_empty())
        .unwrap_or(rows.len());
    rows.drain(..first);
    rows
}

fn expect_transcript(app: &mut App, width: u16, height: u16, golden: &[&str]) {
    let got = transcript(app, width, height);
    let want: Vec<String> = golden.iter().map(|row| (*row).to_string()).collect();
    assert_eq!(
        got,
        want,
        "\n--- got ({width}x{height}) ---\n{}\n--- want ---\n{}",
        got.join("\n"),
        want.join("\n")
    );
}

/// A frame on the one-line (`classic`) footer, so the transcript geometry is stable regardless
/// of the deck preset that ships.
fn app() -> App {
    let mut app = App::new("m", "s");
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
    app
}

fn assistant(text: &str, calls: Vec<ToolCall>) -> Message {
    Message::Assistant {
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(text)]
        },
        tool_calls: calls,
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
        reasoning: Vec::new(),
    }
}

fn expanded(app: &mut App) {
    app.tool_display = ToolDisplay {
        fold: ToolFold::Expanded,
        preview_lines: 5,
    };
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn result(app: &mut App, id: &str, text: &str, structured: serde_json::Value, is_error: bool) {
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: id.into(),
            content: vec![ContentPart::text(text)],
            structured: Some(structured),
            is_error,
            error_code: None,
        },
        meta: Default::default(),
    });
}

fn card(
    app: &mut App,
    name: &str,
    arguments: serde_json::Value,
    text: &str,
    structured: serde_json::Value,
) {
    app.push_message(&assistant("", vec![call("c1", name, arguments)]));
    result(app, "c1", text, structured, false);
}

fn shell_frame() -> App {
    let mut a = app();
    a.push_message(&assistant(
        "Running the checks.",
        vec![
            call("c1", "shell", json!({"command": "make test"})),
            call("c2", "shell", json!({"command": "make lint"})),
            call("c3", "shell", json!({"command": "sleep 5"})),
            call("c4", "shell", json!({"command": "make docs"})),
        ],
    ));
    result(
        &mut a,
        "c1",
        "all good\n2 passed",
        json!({"command": "make test", "exit_code": 0}),
        false,
    );
    result(
        &mut a,
        "c2",
        "boom",
        json!({"command": "make lint", "exit_code": 2}),
        true,
    );
    // c3 has no result: running.
    a.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c4".into(),
            content: vec![],
            structured: Some(json!({"command": "make docs", "cancelled": true})),
            is_error: true,
            error_code: Some("interrupted".into()),
        },
        meta: Default::default(),
    });
    a
}

const SHELL_GOLDEN: &[&str] = &[
    "  Running the checks.",
    "",
    "◻ shell $ make test · exit 0 ✓",
    "│ all good",
    "│ 2 passed",
    "",
    "◻ shell $ make lint · exit 2 ×",
    "│ boom",
    "",
    "• shell $ sleep 5 ⏺",
    "",
    "• shell $ make docs · cancelled",
];

/// The shell shapes (success, failure, running, cancelled) in one frame, at both widths and
/// both built-in palettes: the text is the same under dark and light (only the colours move),
/// which is what the golden pins.
#[test]
fn the_shell_blocks_snapshot_at_80_120_dark_light() {
    for theme in [Theme::dark(), Theme::light()] {
        for width in [80u16, 120] {
            let mut a = shell_frame();
            a.theme = theme.clone();
            expect_transcript(&mut a, width, 24, SHELL_GOLDEN);
        }
    }
}

#[test]
fn the_small_read_is_one_line_snapshot() {
    let mut a = app();
    card(
        &mut a,
        "read",
        json!({"path": "crates/radian-host/src/job_notify.rs", "offset": 99, "limit": 61}),
        "fn main() {}",
        json!(null),
    );
    expect_transcript(
        &mut a,
        80,
        12,
        &["• read crates/radian-host/src/job_notify.rs 100-160 ✓"],
    );
}

#[test]
fn the_collapsed_shell_snapshot_shows_the_head_and_the_hidden_count() {
    let mut a = app();
    a.tool_display = ToolDisplay {
        fold: ToolFold::Collapsed,
        preview_lines: 5,
    };
    let output = (1..=6).map(|i| format!("line {i}\n")).collect::<String>();
    card(
        &mut a,
        "shell",
        json!({"command": "make test"}),
        &output,
        json!({"command": "make test", "exit_code": 0}),
    );
    expect_transcript(
        &mut a,
        80,
        12,
        &["◼ shell $ make test · exit 0 · 6 lines ✓"],
    );
}

fn diff_frame() -> App {
    let mut a = app();
    expanded(&mut a);
    a.push_message(&assistant(
        "Applying the edits.",
        vec![
            call(
                "c1",
                "patch",
                json!({
                    "path": "src/main.rs",
                    "old": "let a = 1;\nlet b = 2;\n",
                    "new": "let a = 1;\nlet b = 2;\nlet c = 3;\n",
                    "replace_all": true,
                }),
            ),
            call(
                "c2",
                "write",
                json!({"path": "a.txt", "content": "one\ntwo\n"}),
            ),
            call(
                "c3",
                "write",
                json!({"path": "b.txt", "content": "three\nfour\n"}),
            ),
            call("c4", "git_diff", json!({})),
            call("c5", "git_diff", json!({})),
        ],
    ));
    result(
        &mut a,
        "c1",
        "replaced 2 occurrences in src/main.rs (matched by exact)",
        json!({
            "path": "src/main.rs",
            "replaced": 2,
            "matched": "exact",
            "hunks": [
                {"old_start": 10, "old_lines": 2, "new_start": 10, "new_lines": 3},
                {"old_start": 40, "old_lines": 2, "new_start": 41, "new_lines": 3},
            ],
        }),
        false,
    );
    result(
        &mut a,
        "c2",
        "wrote 8 bytes to a.txt",
        json!({"path": "a.txt", "bytes_written": 8}),
        false,
    );
    result(
        &mut a,
        "c3",
        "wrote 10 bytes to b.txt",
        json!({"path": "b.txt", "bytes_written": 10, "previous_lines": 5}),
        false,
    );
    let diff = "diff --git a/src/main.rs b/src/main.rs\n\
                --- a/src/main.rs\n\
                +++ b/src/main.rs\n\
                @@ -10,2 +10,2 @@ fn main() {\n\
                \x20let a = 1;\n\
                -let b = 2;\n\
                +let b = 3;\n\
                @@ -20,1 +20,2 @@\n\
                -let c = 4;\n\
                +let c = 5;\n\
                +let d = 6;\n";
    result(&mut a, "c4", diff, json!(null), false);
    result(&mut a, "c5", "-gone\n+added\n same\n", json!(null), false);
    a
}

const DIFF_GOLDEN: &[&str] = &[
    "  Applying the edits.",
    "",
    "◻ patch src/main.rs · +3 −2 ✓",
    "│ @@ -10,2 +10,3 @@",
    "│   10 - let a = 1;",
    "│   11 - let b = 2;",
    "│   10 + let a = 1;",
    "│   11 + let b = 2;",
    "│   12 + let c = 3;",
    "│ @@ -40,2 +41,3 @@",
    "│   40 - let a = 1;",
    "│   41 - let b = 2;",
    "│   41 + let a = 1;",
    "│   42 + let b = 2;",
    "│   43 + let c = 3;",
    "",
    "◻ write a.txt · +2 ✓",
    "│    1 + one",
    "│    2 + two",
    "",
    "◻ write b.txt · +2 −5 ✓",
    "│    1 + three",
    "│    2 + four",
    "",
    "◻ git_diff 1 files +3 −2 ✓",
    "│ diff --git a/src/main.rs b/src/main.rs",
    "│ --- a/src/main.rs",
    "│ +++ b/src/main.rs",
    "│ @@ -10,2 +10,2 @@ fn main() {",
    "│   10   let a = 1;",
    "│   11 - let b = 2;",
    "│   11 + let b = 3;",
    "│ @@ -20,1 +20,2 @@",
    "│   20 - let c = 4;",
    "│   20 + let c = 5;",
    "│   21 + let d = 6;",
    "",
    "◻ git_diff 0 files +1 −1 ✓",
    "│ -gone",
    "│ +added",
    "│ same",
];

/// `patch` (two replaced ranges, the second's `new_start` moved by the first), `write` new and
/// overwrite, `git_diff` multi-hunk and the no-start-line fallback, at both widths and both
/// palettes.
#[test]
fn the_diff_blocks_snapshot_at_80_120_dark_light() {
    for theme in [Theme::dark(), Theme::light()] {
        for width in [80u16, 120] {
            let mut a = diff_frame();
            a.theme = theme.clone();
            expect_transcript(&mut a, width, 60, DIFF_GOLDEN);
        }
    }
}

fn thinking_frame(expanded: bool) -> App {
    let mut a = app();
    a.reasoning_mode = if expanded {
        radian_tui::blocks::ReasoningMode::Expanded
    } else {
        radian_tui::blocks::ReasoningMode::Collapsed
    };
    a.push_message(&Message::Assistant {
        content: vec![ContentPart::text("the answer")],
        tool_calls: Vec::new(),
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
        reasoning: vec![ContentPart::text("first thought\nsecond thought")],
    });
    a
}

#[test]
fn the_thinking_snapshot_expanded_and_folded() {
    for width in [80u16, 120] {
        expect_transcript(
            &mut thinking_frame(true),
            width,
            16,
            &[
                "◻ thinking",
                "│ first thought",
                "│ second thought",
                "",
                "  the answer",
            ],
        );
        expect_transcript(
            &mut thinking_frame(false),
            width,
            16,
            &["◼ thinking · 2 lines", "", "  the answer"],
        );
    }
}

#[test]
fn the_streaming_thinking_snapshot_has_a_spinner_not_a_count() {
    use radian_tui::projection::{SessionProjection, StreamKind};
    use std::time::Instant;
    for expanded in [true, false] {
        for width in [80u16, 120] {
            let mut a = app();
            a.reasoning_mode = if expanded {
                radian_tui::blocks::ReasoningMode::Expanded
            } else {
                radian_tui::blocks::ReasoningMode::Collapsed
            };
            let base = Instant::now();
            a.set_now(base);
            let projection = SessionProjection::new();
            projection.push_delta(StreamKind::Reasoning, "a thought");
            a.update(Input::Stream(projection.snapshot()));
            a.set_now(base);
            let golden: &[&str] = if expanded {
                &["◻ thinking · ⏺", "│ a thought"]
            } else {
                &["◼ thinking · ⏺"]
            };
            expect_transcript(&mut a, width, 16, golden);
        }
    }
}

fn ask_frame(declined: bool) -> App {
    let mut a = app();
    a.push_message(&assistant(
        "",
        vec![call(
            "c1",
            "ask_user",
            json!({"questions":[{"id":"pick","kind":"single","prompt":"Pick one","options":[
                {"id":"a","label":"Option A"},{"id":"b","label":"Option B"}]}]}),
        )],
    ));
    if declined {
        result(&mut a, "c1", "user declined to answer", json!(null), true);
    } else {
        result(
            &mut a,
            "c1",
            "Pick one -> Option A",
            json!({"request_id":"r","answers":[{"id":"pick","selected":["a"]}]}),
            false,
        );
    }
    a
}

/// The record is the open frame (ADR 0028): `╭─ ask …`, `│ ` rows, `╰──` —
/// never a closed box — with the circle markers, and the declined status inside the same frame.
/// The top rule's length tracks the width, so the two widths have their own golden.
#[test]
fn the_ask_record_snapshot_is_an_open_frame() {
    for (width, rule) in [
        (
            80u16,
            "╭─ ask ───────────────────────────────────────────────────────────────────────",
        ),
        (
            120,
            "╭─ ask ───────────────────────────────────────────────────────────────────────────────────────────────────────────────",
        ),
    ] {
        expect_transcript(
            &mut ask_frame(false),
            width,
            16,
            &[rule, "│ Pick one", "│ ◉ Option A", "│ ○ Option B", "╰──"],
        );
        expect_transcript(
            &mut ask_frame(true),
            width,
            16,
            &[
                rule,
                "│ Pick one",
                "│ ○ Option A",
                "│ ○ Option B",
                "│ user declined to answer",
                "╰──",
            ],
        );
    }
}

#[test]
fn the_table_snapshot_keeps_the_header_and_the_cell_text() {
    let mut a = app();
    a.push_message(&assistant(
        "| name | value |\n| --- | --- |\n| `code` | **bold** |\n",
        Vec::new(),
    ));
    for width in [80u16, 120] {
        expect_transcript(
            &mut a,
            width,
            16,
            &[
                "  ┌──────┬───────┐",
                "  │ name │ value │",
                "  ├──────┼───────┤",
                "  │ code │ bold  │",
                "  └──────┴───────┘",
            ],
        );
    }
}

/// A CJK command and output keep the marker column and the body rail (the screen model inserts a
/// cell after each wide glyph, so compare with whitespace squeezed).
#[test]
fn a_cjk_block_keeps_its_marker_column() {
    let mut a = app();
    a.push_message(&assistant(
        "",
        vec![call("c1", "shell", json!({"command": "echo 你好世界"}))],
    ));
    result(
        &mut a,
        "c1",
        "你好，世界\n完成",
        json!({"command": "echo 你好世界", "exit_code": 0}),
        false,
    );
    let got = transcript(&mut a, 80, 12);
    let squeeze = |text: &str| {
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(
        squeeze(&got[0]),
        squeeze("◻ shell $ echo 你好世界 · exit 0 ✓"),
        "{got:?}"
    );
    assert_eq!(squeeze(&got[1]), "│你好，世界", "{got:?}");
    assert_eq!(squeeze(&got[2]), "│完成", "{got:?}");
    // The rail is in columns 0-1 and the body begins at column 2 even with wide glyphs.
    for row in &got[1..] {
        assert!(row.starts_with("│ "), "{row:?}");
    }
}

/// A fixture with a tool body, a failed tool body and an expanded thinking body, for the
/// mechanical rules below.
fn mixed_frame() -> App {
    let mut a = app();
    expanded(&mut a);
    a.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    a.push_message(&Message::Assistant {
        content: vec![ContentPart::text("the answer")],
        tool_calls: vec![
            call("c1", "shell", json!({"command": "make test"})),
            call("c2", "git_diff", json!({})),
        ],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
        reasoning: vec![ContentPart::text("first thought\nsecond thought")],
    });
    result(
        &mut a,
        "c1",
        "one\ntwo",
        json!({"command": "make test", "exit_code": 0}),
        false,
    );
    result(&mut a, "c2", "-old\n+new\n", json!(null), false);
    a
}

/// ADR 0028's mechanical rule: every body row of a tool block and of thinking starts
/// with `│ ` in columns 1-2 and its content begins at column 3, in every built-in theme and at
/// both widths. The head rows are the marker column's (`•`, `◼`, `◻`).
#[test]
fn every_tool_and_thinking_body_row_carries_the_rail_in_any_theme() {
    for theme in [Theme::dark(), Theme::light(), Theme::mono()] {
        for width in [80u16, 120] {
            let mut a = mixed_frame();
            a.theme = theme.clone();
            let height = 24;
            let frame = rows(&render(&mut a, width, height));
            let mut body_rows = 0;
            for row in frame.iter().take(usize::from(height - CHROME)) {
                if let Some(rest) = row.strip_prefix("│ ") {
                    body_rows += 1;
                    assert!(
                        !rest.starts_with(' '),
                        "{theme:?} {width}: the body starts at column 3: {row:?}"
                    );
                }
                // A tool/thinking head never starts with a leading space (the marker column).
                if row.starts_with('•') || row.starts_with('◼') || row.starts_with('◻') {
                    assert!(!row.starts_with(' '), "{row:?}");
                }
            }
            assert!(body_rows >= 4, "{theme:?} {width}: {frame:?}");
        }
    }
}

/// ADR 0028's mechanical rule: no transcript row of any built-in theme carries a
/// background (the thin block is a rail, not a card).
#[test]
fn no_transcript_row_has_a_background_in_any_builtin_theme() {
    for theme in [Theme::dark(), Theme::light(), Theme::mono()] {
        for width in [80u16, 120] {
            let mut a = mixed_frame();
            a.theme = theme.clone();
            let height = 24;
            let terminal = render(&mut a, width, height);
            let buffer = terminal.backend().buffer();
            for y in 0..(height - CHROME) {
                for x in 0..width {
                    let cell = buffer.cell((x, y)).expect("a cell");
                    assert_eq!(
                        cell.bg,
                        Color::Reset,
                        "{theme:?} {width}: row {y} col {x} carries a background"
                    );
                }
            }
        }
    }
}

/// The three-hunk fixture, row by row: a `+` row carries the **new** file's number, a `-` row
/// the **old** file's, and each `@@` sets both counters from its own starts.
#[test]
fn a_three_hunk_diff_numbers_each_row_by_its_own_file() {
    let mut a = app();
    expanded(&mut a);
    let diff = "diff --git a/x.rs b/x.rs\n\
                --- a/x.rs\n\
                +++ b/x.rs\n\
                @@ -5,1 +5,2 @@\n\
                -let a = 1;\n\
                +let a = 2;\n\
                +let a2 = 3;\n\
                @@ -20,1 +21,1 @@\n\
                -let b = 1;\n\
                +let b = 2;\n\
                @@ -40,2 +41,3 @@\n\
                -let c = 1;\n\
                -let c2 = 2;\n\
                +let c = 3;\n\
                +let c2 = 4;\n\
                +let c3 = 5;\n";
    card(&mut a, "git_diff", json!({}), diff, json!(null));
    expect_transcript(
        &mut a,
        80,
        24,
        &[
            "◻ git_diff 1 files +6 −4 ✓",
            "│ diff --git a/x.rs b/x.rs",
            "│ --- a/x.rs",
            "│ +++ b/x.rs",
            "│ @@ -5,1 +5,2 @@",
            "│    5 - let a = 1;",
            "│    5 + let a = 2;",
            "│    6 + let a2 = 3;",
            "│ @@ -20,1 +21,1 @@",
            "│   20 - let b = 1;",
            "│   21 + let b = 2;",
            "│ @@ -40,2 +41,3 @@",
            "│   40 - let c = 1;",
            "│   41 - let c2 = 2;",
            "│   41 + let c = 3;",
            "│   42 + let c2 = 4;",
            "│   43 + let c3 = 5;",
        ],
    );
}

/// ADR 0028's copy rule: a copy of a diff block carries the block's own text — no
/// rail, no head marker, no status glyph, and none of the synthesized gutter's line numbers.
#[test]
fn a_copy_of_a_diff_block_has_no_marker_or_line_number() {
    use radian_tui::blocks;
    use radian_tui::select::{Selection, locate, logical_text};

    let mut a = app();
    expanded(&mut a);
    let diff = "diff --git a/x.rs b/x.rs\n\
                --- a/x.rs\n\
                +++ b/x.rs\n\
                @@ -10,1 +10,1 @@\n\
                -let a = 1;\n\
                +let a = 2;\n";
    card(&mut a, "git_diff", json!({}), diff, json!(null));
    let width = 80usize;
    let modes = a.display_modes();
    let entries = &a.session.entries;
    let call_index = entries
        .iter()
        .position(|entry| entry.kind == EntryKind::ToolCall)
        .expect("the call entry");
    let result_index = entries
        .iter()
        .position(|entry| entry.kind == EntryKind::ToolResult)
        .expect("the result entry");
    let start_line = |index: usize| -> usize {
        entries[..index]
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                blocks::gap_at(entries, i) + render::display_lines(entry, modes, width).len()
            })
            .sum::<usize>()
            + blocks::gap_at(entries, index)
    };
    let first = start_line(call_index);
    let body_lines = render::display_lines(&entries[result_index], modes, width).len();
    let last = start_line(result_index) + body_lines - 1;
    let anchor = locate(entries, modes, width, first, 0).expect("the block's start");
    let head = locate(entries, modes, width, last, 200).expect("the block's end");
    let copied = logical_text(entries, &Selection { anchor, head });

    assert!(copied.contains("let a = 1;"), "{copied:?}");
    assert!(copied.contains("let a = 2;"), "{copied:?}");
    for glyph in ['•', '◼', '◻', '│', OK, FAILED] {
        assert!(!copied.contains(glyph), "{glyph:?} in {copied:?}");
    }
    // No synthesized gutter: every copied line is the diff's own sign + code, never `10 - …`.
    for line in copied.lines() {
        assert!(
            !line.trim_start().starts_with(|c: char| c.is_ascii_digit()),
            "a line number reached the copy: {line:?}"
        );
    }
}
