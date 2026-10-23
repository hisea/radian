//! The thin tool block (ADR 0028): one head line
//! `<fold marker> <tool name> <title> · <meta> <status>` and `│ `-prefixed body rows. The head's
//! fold marker is `•`/`◼`/`◻` (ascii `*`/`+`/`-`), decided by whether a body is drawn under it
//! (ADR 0035's 2026-10-16 "折叠标记统一" note). No background, no half-cell padding, no box, no
//! `⟦…⟧` meta row — the meta is on the head line — and no marker or status glyph ever reaches the
//! text a copy takes.

use radian_core::{ContentPart, Message, ToolCall, ToolResult};
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
/// when the call succeeded, `×` when it failed (the glyph note; U+2717
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

/// Whether a row is a tool block's head row (any of the three fold markers, ADR 0035's
/// 2026-10-16 "折叠标记统一" note): `• ` one-liner, `◼ ` folded, `◻ ` body drawn.
fn is_head_row(row: &str) -> bool {
    ["• ", "◼ ", "◻ "]
        .iter()
        .any(|marker| row.starts_with(marker))
}

/// The rows a tool block occupies: the head row and its body rows.
fn block_rows(lines: &[String]) -> Vec<&String> {
    lines
        .iter()
        .skip_while(|line| !is_head_row(line))
        .take_while(|line| is_head_row(line) || line.starts_with("│ "))
        .collect()
}

/// A `write` call for `a.rs` (`"one\ntwo\nthree\n"`, three lines) plus its result.
fn write_app(result: ToolResult) -> App {
    let mut app = App::new("m", "s");
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: json!({"path": "a.rs", "content": "one\ntwo\nthree\n"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result,
        meta: Default::default(),
    });
    app
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
            block[0], "◻ shell $ make test · exit 0 · 12.4s ✓",
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
    assert_eq!(block[0], "◻ shell $ make test · exit 2 ×", "{block:?}");
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
            &"◻ shell $ make test · exit 0 ✓".to_string(),
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
        vec![&"◼ shell $ make test · exit 0 · 6 lines ✓".to_string()],
        "the head only, with the hidden count"
    );
}

/// ADR 0035's 2026-10-16 "折叠标记统一" note, option A: one fold marker for the thinking header
/// and for an expandable tool head, decided only by whether a body is drawn under the head. Each
/// row of the note's table, at 80 columns, in unicode and ascii:
///
/// * a `collapsed` shell (the `· N lines` head) → `◼ ` / `+ `;
/// * an `errors`-preview shell (the last rows drawn) → `◻ ` / `- `;
/// * a one-line `read` → `• ` / `* `;
/// * a `read` hiding a hundred lines → `• ` / `* ` (a compact tool never folds under `errors`);
/// * the same `read` with `Ctrl-O` (the expanded fold) → `◻ ` / `- `.
#[test]
fn the_tool_head_fold_marker_follows_the_drawn_body() {
    /// One tool call with its result, on the given tier and fold.
    fn tool_app(tier: &str, fold: ToolFold, name: &str, text: &str) -> App {
        let mut app = App::new("m", "s");
        app.apply_footer(&radian_host::config::TuiFooterConfig {
            glyphs: Some(tier.to_string()),
            preset: Some("classic".to_string()),
            ..Default::default()
        });
        app.tool_display = ToolDisplay {
            fold,
            preview_lines: 2,
        };
        app.push_message(&Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: name.into(),
                arguments: json!({"command": "make test", "path": "a.rs"}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        });
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(text)],
                structured: Some(json!({"command": "make test", "exit_code": 0})),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        });
        app
    }

    /// The head row (the first row that is neither empty nor a body rail row) and the body rows.
    fn block(app: &mut App) -> (String, usize) {
        let lines = rows(&render(app, 80, 20), 80, 20);
        let head = lines
            .iter()
            .find(|line| !line.is_empty() && !line.starts_with("│ ") && !line.starts_with("| "))
            .cloned()
            .expect("a head row");
        let body = lines
            .iter()
            .filter(|line| line.starts_with("│ ") || line.starts_with("| "))
            .count();
        (head, body)
    }

    let six = (1..=6).map(|i| format!("line {i}\n")).collect::<String>();
    let hundred = (1..=100).map(|i| format!("line {i}\n")).collect::<String>();
    for (tier, folded, open, dot) in [("unicode", "◼ ", "◻ ", "• "), ("ascii", "+ ", "- ", "* ")]
    {
        // `collapsed`: only the head shows, the six lines named in `· 6 lines` → folded.
        let (head, body) = block(&mut tool_app(tier, ToolFold::Collapsed, "shell", &six));
        assert!(
            head.starts_with(folded),
            "{tier}: collapsed shell: {head:?}"
        );
        assert_eq!(body, 0, "{tier}: a folded block draws no body: {head:?}");

        // `errors`: the last two rows are drawn under the head → open.
        let (head, body) = block(&mut tool_app(tier, ToolFold::Errors, "shell", &six));
        assert!(head.starts_with(open), "{tier}: errors shell: {head:?}");
        assert_eq!(body, 2, "{tier}: the preview rows: {head:?}");

        // A one-line `read`: nothing is folded away → the dot.
        let (head, body) = block(&mut tool_app(
            tier,
            ToolFold::Errors,
            "read",
            "fn main() {}",
        ));
        assert!(head.starts_with(dot), "{tier}: one-line read: {head:?}");
        assert_eq!(body, 0, "{tier}: a one-liner draws no body: {head:?}");

        // A `read` hiding a hundred lines: still the `•` one-liner — a compact tool never folds
        // under `errors`, however many lines its result hides (ADR 0035's 2026-10-16 note, option
        // A; reviewer fix 1: this used to fold to `◼` because the result had more than one line).
        let (head, body) = block(&mut tool_app(tier, ToolFold::Errors, "read", &hundred));
        assert!(head.starts_with(dot), "{tier}: a 100-line read: {head:?}");
        assert!(
            !head.starts_with(folded),
            "{tier}: a read never folds: {head:?}"
        );
        assert_eq!(body, 0, "{tier}: a compact tool draws no body: {head:?}");

        // The same `read` under `Ctrl-O` (the expanded fold): the body is drawn → open.
        let (head, body) = block(&mut tool_app(
            tier,
            ToolFold::Expanded,
            "read",
            "fn main() {}",
        ));
        assert!(head.starts_with(open), "{tier}: read with Ctrl-O: {head:?}");
        assert_eq!(body, 1, "{tier}: the expanded read body: {head:?}");
    }
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
                content: vec![ContentPart::text("fn main() {}")],
                structured: None,
                is_error,
                error_code: None,
            },
            meta: Default::default(),
        });
        app
    };

    // The default fold: one head line, no body. A one-line result is a head-only `•` (ADR
    // 0035's 2026-10-16 note: only a body drawn under the head is `◻`).
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

    // Expanded: the body rows appear under a `◻`.
    let lines = rows(
        &render(&mut read_app(ToolFold::Expanded, false), 80, 12),
        80,
        12,
    );
    let block = block_rows(&lines);
    assert_eq!(block.len(), 2, "{block:?}");
    assert_eq!(
        block[0],
        "◻ read crates/radian-host/src/job_notify.rs 100-160 ✓"
    );
    assert_eq!(block[1], "│ fn main() {}", "{block:?}");

    // An error gives it a body whatever the fold says.
    let lines = rows(
        &render(&mut read_app(ToolFold::Errors, true), 80, 12),
        80,
        12,
    );
    let block = block_rows(&lines);
    assert_eq!(block.len(), 2, "{block:?}");
    assert!(block[0].starts_with('◻'), "{block:?}");
    assert!(block[0].ends_with(FAILED), "{block:?}");
}

/// `write` (ADR 0028): a whole-file write with no hunk, so every content
/// line is a numbered `Added` row. A new file has no `previous_lines`, so the head stays `+M`.
#[test]
fn a_new_file_write_numbers_each_content_line() {
    let mut app = write_app(ToolResult {
        call_id: "c1".into(),
        content: vec![ContentPart::text("wrote 14 bytes to a.rs")],
        structured: Some(json!({"path": "a.rs", "bytes_written": 14})),
        is_error: false,
        error_code: None,
    });
    app.tool_display.fold = ToolFold::Expanded;
    let lines = rows(&render(&mut app, 80, 16), 80, 16);
    let block = block_rows(&lines);
    assert_eq!(block[0], "◻ write a.rs · +3 ✓", "{block:?}");
    // Every content line is an addition, numbered from 1; the `+` is synthesized (the entry
    // text is the content alone).
    assert_eq!(block[1], "│    1 + one", "{block:?}");
    assert_eq!(block[2], "│    2 + two", "{block:?}");
    assert_eq!(block[3], "│    3 + three", "{block:?}");
    assert_eq!(block.len(), 4, "{block:?}");
}

/// An overwrite's real result carries `previous_lines`, so the head shows `+M −N` (the removed
/// side is the old file's line count; the body is only the additions — the old content is not
/// in the result).
#[test]
fn an_overwrite_write_head_shows_the_removed_count() {
    let mut app = write_app(ToolResult {
        call_id: "c1".into(),
        content: vec![ContentPart::text("wrote 14 bytes to a.rs")],
        structured: Some(json!({"path": "a.rs", "bytes_written": 14, "previous_lines": 5})),
        is_error: false,
        error_code: None,
    });
    app.tool_display.fold = ToolFold::Expanded;
    let lines = rows(&render(&mut app, 80, 16), 80, 16);
    let block = block_rows(&lines);
    assert_eq!(block[0], "◻ write a.rs · +3 −5 ✓", "{block:?}");
    assert_eq!(block[1], "│    1 + one", "{block:?}");
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
                "hunks": [{"edit": 1, "old_start": 3, "old_lines": 1, "new_start": 3, "new_lines": 1}],
            })),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let lines = rows(&render(&mut app, 80, 16), 80, 16);
    let block = block_rows(&lines);
    assert_eq!(block[0], "◻ patch src/a.rs · +1 −1 ✓", "{block:?}");
    assert_eq!(block[1], "│ @@ -3,1 +3,1 @@", "{block:?}");
    assert_eq!(block[2], "│    3 - old one", "{block:?}");
    assert_eq!(block[3], "│    3 + new one", "{block:?}");
}

/// A failed `patch` changed nothing: the card body is the tool's own error text (its
/// closest-region hint included), and the head claims no `+N −M` — for the `edits` form too,
/// which is what models send by default. The call's edit text must never be drawn as a change.
#[test]
fn a_failed_patch_shows_the_error_text_and_no_counts() {
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
            arguments: json!({
                "path": "src/a.rs",
                "edits": [
                    {"old": "alpha", "new": "ALPHA"},
                    {"old": "gamma", "new": "GAMMA"}
                ]
            }),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text(
                "`old` does not appear in `src/a.rs`; the file is unchanged\n\
                 The closest region is lines 3-3:\n3: gamma",
            )],
            structured: None,
            is_error: true,
            error_code: Some("patch_not_found".into()),
        },
        meta: Default::default(),
    });
    let lines = rows(&render(&mut app, 80, 16), 80, 16);
    let block = block_rows(&lines);
    assert_eq!(block[0], "◻ patch src/a.rs ×", "{block:?}");
    assert!(
        block[1].contains("does not appear in `src/a.rs`"),
        "{block:?}"
    );
    assert!(
        block[2].starts_with("│ The closest region is lines 3-3"),
        "{block:?}"
    );
    assert!(block[3].starts_with("│ 3: gamma"), "{block:?}");
    for row in &block {
        assert!(
            !row.contains("@@") && !row.contains("-alpha") && !row.contains("−"),
            "a failed patch draws no diff and no counts: {row:?}"
        );
    }
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
        squeeze("◻ shell $ echo 你好世界 · exit 0 ✓"),
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
                if index == 0 || is_head_row(row) {
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
    for unicode in ['•', '◼', '◻', '│'] {
        assert!(!all.contains(unicode), "{unicode:?} in {all}");
    }
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
        .position(|line| is_head_row(line))
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
    for glyph in ['•', '◼', '◻', '│', OK, FAILED, '·'] {
        assert!(!copied.contains(glyph), "{glyph:?} in {copied:?}");
    }
    assert!(
        !copied.contains("exit 0"),
        "the meta never copies: {copied:?}"
    );
}

/// A 40-line heredoc command: `python3 - <<'PY'`, 38 body lines, then `PY` (line 0 plus 39
/// more, so the note reads `… (+39 lines)`).
fn heredoc_command() -> String {
    let mut command = String::from("python3 - <<'PY'\n");
    for i in 1..=38 {
        command.push_str(&format!("print({i})\n"));
    }
    command.push_str("PY");
    command
}

/// The heredoc call, with its result if `result` is `Some`, on the given fold.
fn heredoc_app(fold: ToolFold, result: Option<&str>) -> App {
    let command = heredoc_command();
    let mut app = App::new("m", "s");
    app.tool_display = ToolDisplay {
        fold,
        preview_lines: 2,
    };
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": command}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    if let Some(text) = result {
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(text)],
                structured: Some(json!({"command": command, "exit_code": 0})),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        });
    }
    app
}

/// ADR 0035's 2026-10-16 "多行命令" note, option A: a multi-line command follows its
/// block. The fold marker is on the **first** row only; the command's first line, the
/// `… (+N lines)` note (N the command's remaining lines, never the output's) and the meta are on
/// it; the command's other lines are hidden until the block is fully expanded, then they are
/// `│ ` body rows, before the output.
#[test]
fn a_multiline_command_folds_with_its_block() {
    // Folded (`collapsed`): the head only, the note and the result's hidden count.
    let mut app = heredoc_app(ToolFold::Collapsed, Some("one\ntwo"));
    let lines = rows(&render(&mut app, 80, 50), 80, 50);
    let block = block_rows(&lines);
    assert_eq!(
        block,
        vec![&"◼ shell $ python3 - <<'PY' … (+39 lines) · exit 0 · 2 lines ✓".to_string()],
        "the folded head: the note and the hidden output count"
    );

    // Preview (`errors`): the head with the note, then the output's preview rows only.
    let mut app = heredoc_app(ToolFold::Errors, Some("one\ntwo"));
    let lines = rows(&render(&mut app, 80, 50), 80, 50);
    let block = block_rows(&lines);
    assert_eq!(
        block,
        vec![
            &"◻ shell $ python3 - <<'PY' … (+39 lines) · exit 0 ✓".to_string(),
            &"│ one".to_string(),
            &"│ two".to_string(),
        ],
        "the preview head"
    );

    // Fully expanded (`Ctrl-O`): no note, the command's 39 lines under `│ `, then the output.
    let mut app = heredoc_app(ToolFold::Expanded, Some("one\ntwo"));
    let lines = rows(&render(&mut app, 80, 50), 80, 50);
    let block = block_rows(&lines);
    assert_eq!(
        block[0], "◻ shell $ python3 - <<'PY' · exit 0 ✓",
        "{block:?}"
    );
    assert!(!block[0].contains('…'), "no note when expanded: {block:?}");
    assert_eq!(
        block.len(),
        1 + 39 + 2,
        "the head, 39 command rows, 2 output rows"
    );
    assert_eq!(block[1], "│ print(1)", "{block:?}");
    assert_eq!(block[39], "│ PY", "{block:?}");
    assert_eq!(block[40], "│ one", "{block:?}");
    assert_eq!(block[41], "│ two", "{block:?}");
    // The fold marker is on the first row only.
    assert_eq!(
        block.iter().filter(|row| is_head_row(row)).count(),
        1,
        "one head row: {block:?}"
    );
}

/// A command on one line wider than the row wraps; every continuation row is a `│ ` body row,
/// never a second fold marker (ADR 0035's 2026-10-16 "多行命令" note).
#[test]
fn a_long_single_line_command_wraps_with_rail_continuations() {
    let long = "cargo test --workspace --locked --no-fail-fast -- --nocapture --ignored --exact";
    let mut app = App::new("m", "s");
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": long}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("done")],
            structured: Some(json!({"command": long, "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let lines = rows(&render(&mut app, 40, 20), 40, 20);
    let block = block_rows(&lines);
    assert!(block[0].starts_with("◻ shell $ cargo test"), "{block:?}");
    assert!(block[0].contains("exit 0"), "the meta on row 1: {block:?}");
    assert_eq!(
        block.iter().filter(|row| is_head_row(row)).count(),
        1,
        "one head row: {block:?}"
    );
    assert!(block.len() > 2, "the command wrapped: {block:?}");
    assert!(
        block[1..].iter().all(|row| row.starts_with("│ ")),
        "every continuation is a rail row: {block:?}"
    );
    // The rails stripped, the continuation rows are the command's own tail.
    let joined: String = block[1..]
        .iter()
        .map(|row| row.trim_start_matches("│ "))
        .collect();
    assert!(
        joined.contains("--nocapture"),
        "the wrapped tail: {joined:?}"
    );
}

/// Copying a fully expanded multi-line block yields the command's own lines and the output —
/// no marker, no `│ `, no `… (+N lines)` note (synthesized cells are never copied).
#[test]
fn a_copy_of_a_multiline_command_has_no_marker_rail_or_note() {
    let mut app = heredoc_app(ToolFold::Expanded, Some("one\ntwo"));
    app.set_viewport(60, 60);
    app.set_transcript_width(80);
    let terminal = render(&mut app, 80, 60);
    let lines = rows(&terminal, 80, 60);
    let head = lines
        .iter()
        .position(|line| is_head_row(line))
        .expect("the head row") as u16;
    let last = lines
        .iter()
        .rposition(|line| line.starts_with("│ "))
        .expect("a body row") as u16;
    let _ = app.update(Input::MouseDown { row: head, col: 0 });
    let _ = app.update(Input::MouseDrag { row: last, col: 60 });
    let result = app.update(Input::MouseUp { row: last, col: 60 });
    let copied = result
        .effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Copy { text } => Some(text.clone()),
            _ => None,
        })
        .expect("a copy effect");
    // The command's own lines and the output copy.
    assert!(copied.contains("print(1)"), "{copied:?}");
    assert!(copied.contains("print(38)"), "{copied:?}");
    assert!(copied.contains("PY"), "{copied:?}");
    assert!(
        copied.contains("one") && copied.contains("two"),
        "{copied:?}"
    );
    // The marker, the rail, the note and the meta are synthesized: none of them copies.
    for glyph in ['◼', '◻', '•', '│', '…'] {
        assert!(!copied.contains(glyph), "{glyph:?} in {copied:?}");
    }
    assert!(
        !copied.contains("exit 0"),
        "the meta never copies: {copied:?}"
    );
    assert!(
        !copied.contains("(+39 lines)"),
        "the note never copies: {copied:?}"
    );
}
