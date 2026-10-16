//! The merged tool card (ADR 0028): a call and its result draw one framed block
//! with a head status line, the `$ <command>` section (no divider: it is part of the call)
//! and an `Output` section behind a `├── Output ──┤` divider, a `⟦…⟧` meta row, and frame
//! characters that never enter the text a copy takes.

use agent_core::{ContentPart, Message, ToolCall, ToolResult};
use radian_tui::app::{App, Effect, Input, UpdateResult};
use radian_tui::render;
use radian_tui::tools::{ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
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

/// The status glyphs the app's footer draws (running, success, failure). main's glyph set
/// (ADR 0027/10 + the glyphs branch) is `~`/`+`/`×`, so the tests never hardcode
/// them.
fn status(app: &App) -> [&'static str; 3] {
    app.footer_glyphs().tool_status()
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
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
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A shell call, with its result if `result` is `Some`.
fn shell_app(result: Option<ToolResult>) -> App {
    shell_app_with(result, |_| {})
}

/// `shell_app` with a setup step before any message arrives (the app's real order: the footer
/// glyph set comes from the config at startup, before the first tool result writes its head).
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

#[test]
fn the_card_has_three_sections_at_80_and_120_columns() {
    for width in [80u16, 120] {
        let mut app = shell_app(Some(ok_result("all good\n12 passed")));
        let ok = status(&app)[1];
        let terminal = render(&mut app, width, 16);
        let lines = rows(&terminal, width, 16);
        let w = width as usize;
        let inner = w - 2;
        assert_eq!(lines[0], "", "the container's gap");
        assert_eq!(
            lines[1],
            format!("┌{}┐", "─".repeat(inner)),
            "the top border at {width} columns"
        );
        // The head status line: the glyph, `shell:` and the title (revision 3 rule 1).
        assert!(
            lines[2].starts_with(&format!("│ {ok} shell: make test")),
            "{}",
            lines[2]
        );
        assert!(lines[2].ends_with('│'), "{}", lines[2]);
        // The command section carries no divider (omp's first section has no label, and the
        // command is part of the call): the head and the command read as one unit.
        assert!(lines[3].starts_with("│ $ make test"), "{}", lines[3]);
        assert_eq!(
            lines[4],
            format!("├── Output {}┤", "─".repeat(inner - 10)),
            "the output divider"
        );
        assert!(lines[5].starts_with("│ all good"), "{}", lines[5]);
        assert!(lines[6].starts_with("│ 12 passed"), "{}", lines[6]);
        // The meta row: the brackets from the glyph set, the fields that exist.
        assert!(
            lines[7].starts_with("│ ⟦exit 0 | timeout 30s⟧"),
            "{}",
            lines[7]
        );
        assert_eq!(
            lines[8],
            format!("└{}┘", "─".repeat(inner)),
            "the bottom border"
        );
    }
}

#[test]
fn a_failed_call_labels_its_section_error() {
    let mut app = shell_app(Some(ToolResult {
        call_id: "c1".into(),
        content: vec![ContentPart::text("boom")],
        structured: Some(json!({"command": "make test", "exit_code": 2})),
        is_error: true,
        error_code: None,
    }));
    let failed = status(&app)[2];
    let terminal = render(&mut app, 80, 20);
    let text = screen(&terminal);
    assert!(
        text.contains(&format!("{failed} shell: make test")),
        "{text}"
    );
    assert!(text.contains("├── Error "), "{text}");
    assert!(!text.contains("├── Output "), "{text}");
    // The error band covers the whole card (ADR 0028 rule 3). The card starts
    // below the container's gap and its row indices move with the input style's height
    // (ADR 0035 V3), so find its top and bottom borders instead of hardcoding rows.
    let buffer = terminal.backend().buffer();
    let symbol = |y: u16| buffer.cell((0, y)).map(|c| c.symbol().to_string());
    let top = (0..20)
        .find(|&y| symbol(y).as_deref() == Some("┌"))
        .expect("top border");
    let bottom = (0..20)
        .rev()
        .find(|&y| symbol(y).as_deref() == Some("└"))
        .expect("bottom border");
    let band = buffer.cell((0, top)).map(|c| c.bg);
    assert!(band.is_some(), "the card carries a band");
    assert_eq!(
        buffer.cell((0, bottom)).map(|c| c.bg),
        band,
        "…to the last row"
    );
}

#[test]
fn a_call_with_no_output_draws_no_output_section() {
    let mut app = shell_app(Some(ok_result("")));
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    assert!(
        text.contains("$ make test"),
        "the command section stays: {text}"
    );
    assert!(!text.contains("├── "), "no output, no divider: {text}");
}

#[test]
fn a_running_call_is_a_card_with_a_spinner_and_no_output_section() {
    let mut app = shell_app(None);
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    // The running glyph is the live spinner frame (ADR 0027: the default pulse
    // draws `⏺`); the renderer substitutes it for the status glyph baked at call time.
    let head = text
        .lines()
        .find(|l| l.contains("shell: make test"))
        .unwrap();
    assert!(head.starts_with("│ "), "{head}");
    assert!(head.contains('⏺'), "the running spinner: {head}");
    assert!(text.contains("$ make test"), "the command section: {text}");
    assert!(!text.contains("├── "), "no output, no divider: {text}");
    assert!(text.contains('└'), "the card closes: {text}");
}

#[test]
fn a_cancelled_call_says_cancelled_in_its_meta() {
    let mut app = shell_app(Some(ToolResult {
        call_id: "c1".into(),
        content: vec![],
        structured: Some(json!({"command": "make test", "cancelled": true})),
        is_error: true,
        error_code: Some("interrupted".into()),
    }));
    let terminal = render(&mut app, 80, 12);
    assert!(
        screen(&terminal).contains("⟦cancelled⟧"),
        "{}",
        screen(&terminal)
    );
}

#[test]
fn a_folded_card_keeps_its_head_and_its_meta() {
    let mut app = shell_app(Some(ok_result(
        "line 1\nline 2\nline 3\nline 4\nline 5\nline 6",
    )));
    app.tool_display = ToolDisplay {
        fold: ToolFold::Collapsed,
        preview_lines: 2,
    };
    let ok = status(&app)[1];
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    assert!(text.contains(&format!("{ok} shell: make test")), "{text}");
    assert!(
        text.contains("$ make test"),
        "the command is the call: {text}"
    );
    assert!(
        !text.contains("├── "),
        "a folded hint is no section: {text}"
    );
    assert!(text.contains("[6 lines]"), "the folded hint: {text}");
    assert!(text.contains("⟦exit 0"), "the meta stays: {text}");
}

#[test]
fn a_multi_line_command_wraps_as_is() {
    let mut app = App::new("m", "s");
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": "make test\nmake lint"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    assert!(text.contains("│ $ make test"), "{text}");
    assert!(
        text.contains("│ make lint"),
        "the second command line: {text}"
    );
}

#[test]
fn other_tools_are_their_head_and_their_output_section() {
    let mut app = App::new("m", "s");
    // `read` folds to a hint by default; the section shows under `expanded`.
    app.tool_display = ToolDisplay {
        fold: ToolFold::Expanded,
        preview_lines: 5,
    };
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "read".into(),
            arguments: json!({"path": "a.rs"}),
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
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let terminal = render(&mut app, 80, 12);
    let ok = status(&app)[1];
    let text = screen(&terminal);
    assert!(text.contains(&format!("{ok} read: a.rs")), "{text}");
    assert!(
        !text.contains("Command"),
        "no shell command section: {text}"
    );
    assert!(text.contains("├── Output "), "{text}");
    assert!(text.contains("fn main() {}"), "{text}");
    // The meta row only writes the data that exists: `read` has no exit code, no duration, no
    // timeout, so there is no meta row at all (ADR 0028 rule 3).
    assert!(!text.contains("exit"), "no invented exit code: {text}");
    assert!(!text.contains('⟦'), "no empty meta row: {text}");
}

#[test]
fn the_meta_writes_only_the_data_that_exists() {
    // A measured duration is written; an unmeasured one is not (the shell card above has
    // none and shows none).
    let mut app = shell_app(Some(ok_result("done")));
    if let Some(entry) = app
        .session
        .entries
        .iter_mut()
        .find(|e| e.kind == radian_tui::app::EntryKind::ToolResult)
    {
        entry.tool.as_mut().expect("the card").duration_ms = Some(50);
    }
    let terminal = render(&mut app, 80, 12);
    assert!(
        screen(&terminal).contains("⟦exit 0 | 0.05s | timeout 30s⟧"),
        "{}",
        screen(&terminal)
    );
    // A shell card with no timeout argument writes no timeout.
    let mut app = App::new("m", "s");
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: json!({"command": "true"}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("")],
            structured: Some(json!({"command": "true", "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let terminal = render(&mut app, 80, 12);
    assert!(
        screen(&terminal).contains("⟦exit 0⟧"),
        "only the exit code: {}",
        screen(&terminal)
    );
    assert!(
        !screen(&terminal).contains("timeout"),
        "{}",
        screen(&terminal)
    );
}

#[test]
fn the_ascii_glyph_set_draws_ascii_borders_and_brackets() {
    // The glyph set is chosen before the result arrives, as at startup: the head's status glyph
    // is written into the entry when the result lands (app.rs), so a later switch would not
    // repaint it (a separate, known limit).
    let mut app = shell_app_with(Some(ok_result("done")), |app| {
        app.apply_footer(&radian_host::config::TuiFooterConfig {
            glyphs: Some("ascii".into()),
            ..Default::default()
        });
    });
    let terminal = render(&mut app, 60, 12);
    let lines = rows(&terminal, 60, 12);
    // The card's own rows (the footer draws its own separators, not this assertion's).
    let card: Vec<&String> = lines
        .iter()
        .skip_while(|l| !l.starts_with('+'))
        .take_while(|l| l.starts_with('+') || l.starts_with('|'))
        .collect();
    let text = card
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let ok = status(&app)[1];
    assert!(text.contains(&format!("| {ok} shell: make test")), "{text}");
    assert!(text.contains("| $ make test"), "{text}");
    assert!(text.contains("+-- Output "), "{text}");
    assert!(text.contains("[exit 0 | timeout 30s]"), "{text}");
    assert!(!text.contains('┌') && !text.contains('│'), "{text}");
}

#[test]
fn a_cjk_command_and_output_fit_the_frame() {
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
    let terminal = render(&mut app, 40, 12);
    let lines = rows(&terminal, 40, 12);
    // Every frame row is exactly 40 columns wide (the CJK glyphs are two cells).
    for line in lines.iter().take_while(|l| !l.is_empty()) {
        if line.starts_with('│') || line.starts_with('┌') || line.starts_with('├') {
            assert_eq!(line.chars().count(), 40, "{line:?}");
        }
    }
}

#[test]
fn a_copy_of_a_card_has_no_border_characters() {
    let mut app = shell_app(Some(ok_result("all good")));
    app.set_viewport(20, 20);
    app.set_transcript_width(80);
    // The card is lines 0 (top) .. 7 (bottom): select from the head row's first text column
    // to the last output row.
    // The head row's on-screen index (the view follows the bottom, so it may be scrolled).
    let terminal = render(&mut app, 80, 20);
    let head_row = rows(&terminal, 80, 20)
        .iter()
        .position(|line| line.contains("shell: make test"))
        .expect("the head row") as u16;
    let _down: UpdateResult = app.update(Input::MouseDown {
        row: head_row,
        col: 2,
    });
    let _ = app.update(Input::MouseDrag {
        row: head_row + 5,
        col: 12,
    });
    let result = app.update(Input::MouseUp {
        row: head_row + 5,
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
    assert!(!copied.contains('│'), "no side border: {copied:?}");
    assert!(!copied.contains('─'), "no horizontal border: {copied:?}");
    assert!(!copied.contains('┌'), "no corner: {copied:?}");
    assert!(copied.contains("make test"), "{copied:?}");
}
