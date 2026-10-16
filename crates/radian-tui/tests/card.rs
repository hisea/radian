//! The thin tool block (ADR 0028): one head line
//! `• <tool name> <title> · <meta> <status>` and `│ `-prefixed body rows. No background, no
//! half-cell padding, no box, no `⟦…⟧` meta row — the meta is on the head line — and no
//! marker or status glyph ever reaches the text a copy takes.

use agent_core::{ContentPart, Message, ToolCall, ToolResult};
use radian_tui::app::{App, Effect, EntryKind, Input, UpdateResult};
use radian_tui::render;
use radian_tui::theme::Theme;
use radian_tui::tools::{ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use serde_json::json;

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render::draw(f, app)).unwrap();
    terminal
}

fn rows(terminal: &Terminal<TestBackend>, width: u16, height: u16) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
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

/// The status glyphs the transcript's head ends with (ADR 0028): `✓`
/// when the call succeeded, `×` when it failed (the owner's 2026-10-16 glyph note; U+2717
/// misses Cascadia Code, Fira Code and Source Code Pro). The `ascii` tier draws `+`/`x`.
const OK: char = '✓';
const FAILED: char = '×';

/// A shell call, with its result if `result` is `Some`.
fn shell_app(result: Option<ToolResult>) -> App {
    shell_app_with(result, |_| {})
}

/// `shell_app` with a setup step before any message arrives (the app's real order: the glyph
/// set comes from the config at startup).
fn shell_app_with(result: Option<ToolResult>, setup: impl FnOnce(&mut App)) -> App {
    let mut app = App::new("m", "s");
    setup(&mut app);
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": "make test", "timeout_secs": 30}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    if let Some(result) = result {
        app.push_message(&Message::Tool {
            result,
            meta: Default::default(),
        });
    }
    app
}

fn ok_result(text: &str) -> ToolResult {
    ToolResult {
        call_id: "c1".into(),
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "command": "make test",
            "exit_code": 0,
            "timeout_secs": 30,
        })),
        is_error: false,
        error_code: None,
    }
}

/// Sets the (producer-less) measured duration on the call's card, so the meta shows one.
fn with_duration(app: &mut App, ms: u64) {
    if let Some(entry) = app
        .session
        .entries
        .iter_mut()
        .find(|entry| entry.kind == EntryKind::ToolCall)
        && let Some(card) = entry.tool.as_mut()
    {
        card.duration_ms = Some(ms);
    }
}

/// The rows a tool block occupies: the head row and its body rows.
fn block_rows(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .skip_while(|line| !line.starts_with("• "))
        .take_while(|line| line.starts_with("• ") || line.starts_with("│ "))
        .collect()
}

#[test]
fn the_head_line_is_the_name_the_title_the_meta_and_the_status() {
    for width in [80u16, 120] {
        let mut app = shell_app(Some(ok_result("all good\n12 passed")));
        with_duration(&mut app, 12400);
        let terminal = render(&mut app, width, 16);
        let lines = rows(&terminal, width, 16);
        let block = block_rows(&lines);
        assert_eq!(
            block[0], "• shell $ make test · exit 0 · 12.4s ✓",
            "the head at {width} columns"
        );
        // The output rows: `│ ` and the text from column 3.
        assert_eq!(block[1], "│ all good", "{block:?}");
        assert_eq!(block[2], "│ 12 passed", "{block:?}");
        assert_eq!(block.len(), 3, "no other row: {block:?}");
        // No box, no divider, no band and no `⟦…⟧` meta row anywhere.
        let all = lines.join("\n");
        for glyph in ['┌', '┐', '└', '┘', '├', '┤', '⟦', '⟧'] {
            assert!(!all.contains(glyph), "{glyph:?} is gone: {all}");
        }
    }
}

#[test]
fn a_failed_call_ends_the_head_with_a_cross_and_shows_the_error_body() {
    let mut app = shell_app(Some(ToolResult {
        call_id: "c1".into(),
        content: vec![ContentPart::text("boom")],
        structured: Some(json!({"command": "make test", "exit_code": 2})),
        is_error: true,
        error_code: None,
    }));
    let terminal = render(&mut app, 80, 20);
    let lines = rows(&terminal, 80, 20);
    let block = block_rows(&lines);
    assert_eq!(block[0], "• shell $ make test · exit 2 ×", "{block:?}");
    assert_eq!(block[1], "│ boom", "the error body: {block:?}");
    // The head's cross and the error body carry the error roles.
    let buffer = terminal.backend().buffer();
    let cell_of = |ch: char| {
        let (x, y) = (0..80u16)
            .flat_map(|y| (0..80u16).map(move |x| (x, y)))
            .find(|&(x, y)| {
                buffer.cell((x, y)).map(|c| c.symbol()) == Some(ch.to_string().as_str())
            })
            .unwrap_or_else(|| panic!("{ch:?} is on screen"));
        buffer.cell((x, y)).map(|c| c.fg)
    };
    assert_eq!(cell_of(FAILED), app.theme.status_error.fg, "the `×` role");
    assert_eq!(
        cell_of('b'),
        app.theme.tool_result_error.fg,
        "the error body's role"
    );
}

#[test]
fn a_running_call_ends_the_head_with_the_pulse_and_shows_no_body() {
    let mut app = shell_app(None);
    let terminal = render(&mut app, 80, 12);
    let lines = rows(&terminal, 80, 12);
    let block = block_rows(&lines);
    assert_eq!(block.len(), 1, "one head line: {block:?}");
    assert!(block[0].starts_with("• shell $ make test"), "{block:?}");
    assert!(block[0].ends_with('⏺'), "the pulse at the end: {block:?}");
    assert!(
        !block[0].contains('·'),
        "no meta while the call runs: {block:?}"
    );
}

#[test]
fn a_cancelled_call_says_cancelled_in_its_meta_and_draws_no_status_glyph() {
    let mut app = shell_app(Some(ToolResult {
        call_id: "c1".into(),
        content: vec![],
        structured: Some(json!({"command": "make test", "cancelled": true})),
        is_error: true,
        error_code: Some("interrupted".into()),
    }));
    let terminal = render(&mut app, 80, 12);
    let lines = rows(&terminal, 80, 12);
    let block = block_rows(&lines);
    assert_eq!(block[0], "• shell $ make test · cancelled", "{block:?}");
    assert!(!block[0].contains(OK), "{block:?}");
    assert!(!block[0].contains(FAILED), "{block:?}");
}

/// The three fold modes (ADR 0028): `expanded` everything, `errors` the
/// last `tool_preview_lines` rows, `collapsed` no body at all plus `· N lines` in the meta.
#[test]
fn the_fold_modes_decide_the_body_and_the_collapsed_meta() {
    let output = (1..=6).map(|i| format!("line {i}\n")).collect::<String>();
    let mut app = shell_app(Some(ok_result(&output)));
    app.tool_display = ToolDisplay {
        fold: ToolFold::Expanded,
        preview_lines: 5,
    };
    let lines = rows(&render(&mut app, 80, 20), 80, 20);
    let block = block_rows(&lines);
    assert_eq!(block.len(), 7, "the head and every output row: {block:?}");
    assert_eq!(block[6], "│ line 6", "{block:?}");

    let mut app = shell_app(Some(ok_result(&output)));
    app.tool_display = ToolDisplay {
        fold: ToolFold::Errors,
        preview_lines: 2,
    };
    let lines = rows(&render(&mut app, 80, 20), 80, 20);
    let block = block_rows(&lines);
    assert_eq!(
        block,
        vec![
            &"• shell $ make test · exit 0 ✓".to_string(),
            &"│ line 5".to_string(),
            &"│ line 6".to_string(),
        ],
        "the last preview rows only"
    );

    let mut app = shell_app(Some(ok_result(&output)));
    app.tool_display = ToolDisplay {
        fold: ToolFold::Collapsed,
        preview_lines: 2,
    };
    let lines = rows(&render(&mut app, 80, 20), 80, 20);
    let block = block_rows(&lines);
    assert_eq!(
        block,
        vec![&"• shell $ make test · exit 0 · 6 lines ✓".to_string()],
        "the head only, with the hidden count"
    );
}

/// `read` is a small tool (ADR 0028): a successful call is one line, and
/// `Ctrl-O` (the expanded fold) or an error gives it a body.
#[test]
fn a_small_tool_is_one_line_until_it_is_expanded() {
    let read_app = |fold: ToolFold, is_error: bool| {
        let mut app = App::new("m", "s");
        app.tool_display = ToolDisplay {
            fold,
            preview_lines: 5,
        };
        app.push_message(&Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: json!({"path": "crates/radian-host/src/job_notify.rs", "offset": 99, "limit": 61}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        });
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text("fn main() {}\nsecond line")],
                structured: None,
                is_error,
                error_code: None,
            },
            meta: Default::default(),
        });
        app
    };

    // The default fold: one head line, no body.
    let lines = rows(
        &render(&mut read_app(ToolFold::Errors, false), 80, 12),
        80,
        12,
    );
    let block = block_rows(&lines);
    assert_eq!(
        block,
        vec![&"• read crates/radian-host/src/job_notify.rs 100-160 ✓".to_string()],
        "a read one-liner"
    );

    // Expanded: the body rows appear.
    let lines = rows(
        &render(&mut read_app(ToolFold::Expanded, false), 80, 12),
        80,
        12,
    );
    let block = block_rows(&lines);
    assert_eq!(block.len(), 3, "{block:?}");
    assert_eq!(block[1], "│ fn main() {}", "{block:?}");

    // An error gives it a body whatever the fold says.
    let lines = rows(
        &render(&mut read_app(ToolFold::Errors, true), 80, 12),
        80,
        12,
    );
    let block = block_rows(&lines);
    assert_eq!(block.len(), 3, "{block:?}");
    assert!(block[0].ends_with(FAILED), "{block:?}");
}

/// `patch` shows its replacement as a diff body: `│ ` rows with the gutter, the sign and the
/// code (slice ④'s row builder, kept).
#[test]
fn a_patch_block_shows_its_diff_body() {
    let mut app = App::new("m", "s");
    app.tool_display = ToolDisplay {
        fold: ToolFold::Expanded,
        preview_lines: 5,
    };
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: json!({"path": "src/a.rs", "old": "old one\n", "new": "new one\n"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("replaced 1 occurrence in src/a.rs")],
            structured: Some(json!({
                "path": "src/a.rs",
                "replaced": 1,
                "matched": "exact",
                "hunks": [{"old_start": 3, "old_lines": 1, "new_start": 3, "new_lines": 1}],
            })),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let lines = rows(&render(&mut app, 80, 16), 80, 16);
    let block = block_rows(&lines);
    assert_eq!(block[0], "• patch src/a.rs · +1 −1 ✓", "{block:?}");
    assert_eq!(block[1], "│ @@ -3,1 +3,1 @@", "{block:?}");
    assert_eq!(block[2], "│    3 - old one", "{block:?}");
    assert_eq!(block[3], "│    3 + new one", "{block:?}");
}

/// The count mirror: `blocks::line_count` equals the rows `blocks::layout` draws for a tool
/// block, at two widths and in every fold mode (virtualization measures without laying out).
#[test]
fn line_count_matches_the_layout_for_a_tool_block() {
    use radian_tui::blocks;
    for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
        for width in [80usize, 24] {
            let mut app = App::new("m", "s");
            app.tool_display = ToolDisplay {
                fold,
                preview_lines: 3,
            };
            app.push_message(&Message::Assistant {
                content: vec![],
                reasoning: vec![],
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: json!({"command": "make test\nmake lint --all --long"}),
                }],
                provider_payload: None,
                interrupted: false,
                meta: Default::default(),
            });
            app.push_message(&Message::Tool {
                result: ToolResult {
                    call_id: "c1".into(),
                    content: vec![ContentPart::text(
                        (1..=9).map(|i| format!("line {i}\n")).collect::<String>(),
                    )],
                    structured: Some(json!({"command": "make test", "exit_code": 0})),
                    is_error: false,
                    error_code: None,
                },
                meta: Default::default(),
            });
            let modes = app.display_modes();
            for entry in &app.session.entries {
                // A tool entry has no fenced blocks (only assistant prose does).
                let count = blocks::line_count(entry, modes, width);
                let laid_out = blocks::layout(entry, &[], modes, width).len();
                assert_eq!(count, laid_out, "{fold:?} {width} {entry:?}");
            }
        }
    }
}

#[test]
fn a_cjk_command_and_output_keep_the_marker_column() {
    let mut app = App::new("m", "s");
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": "echo 你好世界"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("你好，世界\n完成")],
            structured: Some(json!({"command": "echo 你好世界", "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let lines = rows(&render(&mut app, 40, 12), 40, 12);
    let block = block_rows(&lines);
    assert_eq!(block.len(), 3, "{block:?}");
    // The screen model inserts a blank cell after each wide glyph, so compare without spaces.
    let squeeze = |text: &str| {
        text.chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
    };
    assert_eq!(
        squeeze(block[0]),
        squeeze("• shell $ echo 你好世界 · exit 0 ✓"),
        "{block:?}"
    );
    assert_eq!(squeeze(block[1]), "│你好，世界", "{block:?}");
    assert_eq!(squeeze(block[2]), "│完成", "{block:?}");
}

/// The mechanical rules of the thin block (ADR 0028's acceptance, ADR 0040's column
/// rule): no cell of a tool row carries a background, every body row starts with `│ ` and its
/// content begins at column 3, and the head ends with the status glyph.
#[test]
fn every_tool_row_has_no_background_and_the_marker_column_holds() {
    let long = "cargo test --workspace --locked --no-fail-fast -- --nocapture";
    for theme in [Theme::dark(), Theme::light()] {
        for width in [80u16, 120] {
            let mut app = App::new("m", "s");
            app.theme = theme.clone();
            app.push_message(&Message::Assistant {
                content: vec![],
                reasoning: vec![],
                tool_calls: vec![
                    ToolCall {
                        id: "c1".into(),
                        name: "shell".into(),
                        arguments: json!({"command": long}),
                    },
                    ToolCall {
                        id: "c2".into(),
                        name: "read".into(),
                        arguments: json!({"path": "src/main.rs"}),
                    },
                ],
                provider_payload: None,
                interrupted: false,
                meta: Default::default(),
            });
            for (id, text, is_error) in [
                ("c1", format!("all good\n{}", "y".repeat(120)), false),
                ("c2", "fn main() {}".to_string(), false),
            ] {
                app.push_message(&Message::Tool {
                    result: ToolResult {
                        call_id: id.into(),
                        content: vec![ContentPart::text(text)],
                        structured: (id == "c1").then(|| json!({"command": long, "exit_code": 0})),
                        is_error,
                        error_code: None,
                    },
                    meta: Default::default(),
                });
            }
            let height = 20;
            let terminal = render(&mut app, width, height);
            let lines = rows(&terminal, width, height);
            let buffer = terminal.backend().buffer();
            let block = block_rows(&lines);
            assert!(block.len() >= 3, "{block:?}");
            // The head row ends with the status glyph; every other row is a `│ ` body row
            // whose content starts at column 3.
            for (index, row) in block.iter().enumerate() {
                let y = lines.iter().position(|line| line == *row).unwrap() as u16;
                if index == 0 || row.starts_with("• ") {
                    assert!(row.ends_with(OK), "{row:?}");
                } else {
                    assert!(row.starts_with("│ "), "{row:?}");
                    assert_ne!(
                        row.chars().nth(2),
                        Some(' '),
                        "the content starts at column 3: {row:?}"
                    );
                }
                for x in 0..width {
                    let cell = buffer.cell((x, y)).expect("a cell");
                    assert_eq!(
                        cell.bg,
                        Color::Reset,
                        "{theme:?} {width} row {y} col {x} carries a background"
                    );
                }
            }
        }
    }
}

#[test]
fn the_ascii_glyph_set_draws_ascii_markers_and_status() {
    let mut app = shell_app_with(Some(ok_result("done")), |app| {
        app.apply_footer(&radian_host::config::TuiFooterConfig {
            glyphs: Some("ascii".into()),
            ..Default::default()
        });
    });
    let lines = rows(&render(&mut app, 60, 12), 60, 12);
    let head = lines
        .iter()
        .position(|line| line.starts_with("- shell"))
        .expect("the ascii head row");
    assert_eq!(lines[head], "- shell $ make test · exit 0 +", "{lines:?}");
    assert_eq!(lines[head + 1], "| done", "{lines:?}");
    let all = lines.join("\n");
    assert!(!all.contains('•'), "{all}");
    assert!(!all.contains('│'), "{all}");
}

#[test]
fn a_copy_of_a_block_has_no_marker_meta_or_status() {
    let mut app = shell_app(Some(ok_result("all good")));
    with_duration(&mut app, 30);
    app.set_viewport(20, 20);
    app.set_transcript_width(80);
    let terminal = render(&mut app, 80, 20);
    let head_row = rows(&terminal, 80, 20)
        .iter()
        .position(|line| line.starts_with("• shell"))
        .expect("the head row") as u16;
    let _down: UpdateResult = app.update(Input::MouseDown {
        row: head_row,
        col: 0,
    });
    let _ = app.update(Input::MouseDrag {
        row: head_row + 1,
        col: 12,
    });
    let result = app.update(Input::MouseUp {
        row: head_row + 1,
        col: 12,
    });
    let copied = result
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Copy { text } => Some(text.clone()),
            _ => None,
        })
        .expect("a copy effect");
    // The head's own words and the body's output copy; the markers, the meta and the status
    // glyph do not.
    assert!(copied.contains("make test"), "{copied:?}");
    assert!(copied.contains("all good"), "{copied:?}");
    for glyph in ['•', '│', OK, FAILED, '·'] {
        assert!(!copied.contains(glyph), "{glyph:?} in {copied:?}");
    }
    assert!(
        !copied.contains("exit 0"),
        "the meta never copies: {copied:?}"
    );
}
