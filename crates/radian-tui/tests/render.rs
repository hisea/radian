//! Rendering tests on `TestBackend`: reasoning and assistant text are distinguishable,
//! an interrupted prefix is marked, and tool calls are shown.

use radian_coding::QuestionKind;
use radian_core::{AgentEvent, ContentPart, Message, ToolCall, ToolResult};
use radian_host::{PendingRequest, PendingView};
use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::blocks::ReasoningMode;
use radian_tui::projection::StreamKind;
use radian_tui::render;
use radian_tui::tools::DisplayModes;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

/// One streaming chunk the way the loop does it: the projection accumulates the delta, then the
/// snapshot becomes the live entries (ADR 0023 U2).
fn stream(
    app: &mut App,
    projection: &radian_tui::projection::SessionProjection,
    kind: radian_tui::projection::StreamKind,
    text: &str,
) {
    projection.push_delta(kind, text);
    app.update(radian_tui::app::Input::Stream(projection.snapshot()));
}

/// Puts a test on the pre-ADR one-line status bar (`preset = classic`). The footer default is
/// the two-row `pi` layout now (ADR 0027 C2); a test about transcript geometry keeps the
/// layout it was written for, rather than shifting its expected rows.
fn classic_footer(app: &mut App) {
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
}

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
            // `col` is a byte offset; the buffer column is the display width of the prefix
            // (wide cells shift it, so `↑` or CJK would otherwise read the wrong cell).
            let x = area.left() + unicode_width::UnicodeWidthStr::width(&line[..col]) as u16;
            return buffer.cell((x, y)).map(|c| c.style());
        }
    }
    None
}

fn assistant_with(
    content: &str,
    reasoning: &str,
    interrupted: bool,
    calls: Vec<ToolCall>,
) -> Message {
    Message::Assistant {
        content: if content.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(content)]
        },
        tool_calls: calls,
        provider_payload: None,
        interrupted,
        meta: Default::default(),
        reasoning: if reasoning.is_empty() {
            Vec::new()
        } else {
            vec![ContentPart::text(reasoning)]
        },
    }
}

#[test]
fn reasoning_and_assistant_text_are_visually_distinct() {
    let mut app = App::new("test-model", "s1");
    // Thinking is collapsed by default (ADR 0028 R3): this test is about the two styles
    // when the reasoning text is shown at all.
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    // Live deltas arrive first (through the projection, exactly as the loop feeds them)...
    let projection = radian_tui::projection::SessionProjection::new();
    stream(
        &mut app,
        &projection,
        StreamKind::Reasoning,
        "THINKING-WORDS",
    );
    stream(
        &mut app,
        &projection,
        StreamKind::Assistant,
        "VISIBLE-ANSWER",
    );
    // ...then the committed message replaces the live view.
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("VISIBLE-ANSWER", "THINKING-WORDS", false, vec![]),
    });

    let terminal = render(&mut app, 60, 14);
    let text = screen(&terminal);
    assert!(text.contains("VISIBLE-ANSWER"), "{text}");
    assert!(text.contains("THINKING-WORDS"), "{text}");

    let reasoning = style_of(&terminal, "THINKING-WORDS").expect("reasoning on screen");
    let answer = style_of(&terminal, "VISIBLE-ANSWER").expect("answer on screen");
    assert_eq!(reasoning.fg, Some(Color::Rgb(150, 160, 164)));
    assert_ne!(
        reasoning, answer,
        "reasoning must not share the assistant text's style"
    );
    assert!(
        !reasoning
            .add_modifier
            .contains(ratatui::style::Modifier::ITALIC),
        "reasoning is upright now (ADR 0029): {reasoning:?}"
    );
}

#[test]
fn an_interrupted_prefix_is_marked() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("half an ans", "", true, vec![]),
    });
    let terminal = render(&mut app, 60, 12);
    let text = screen(&terminal);
    assert!(text.contains("half an ans"), "{text}");
    assert!(text.contains("[interrupted]"), "{text}");
    let marker = style_of(&terminal, " [interrupted]").unwrap();
    assert_eq!(
        marker.fg,
        Some(Color::Rgb(205, 154, 34)),
        "pi's warning level"
    );
}

#[test]
fn tool_calls_and_results_are_shown() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with(
            "",
            "",
            false,
            vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            }],
        ),
    });
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 2,
        message: Message::Tool {
            result: ToolResult::success("c1", vec![ContentPart::text("file contents")]),
            meta: Default::default(),
        },
    });
    let terminal = render(&mut app, 60, 14);
    let text = screen(&terminal);
    assert!(text.contains("read_file"), "{text}");
    assert!(text.contains("file contents"), "{text}");
}

/// The footer's note must not stay at "tool X running" after the tool finished: the next
/// model request is in flight then, and the note would be stale for the rest of the turn.
#[test]
fn the_note_clears_when_the_tools_finish() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::ToolStarted {
        index: 0,
        call_id: "c1".into(),
        name: "shell".into(),
    });
    assert_eq!(app.session.note.as_deref(), Some("tool shell running"));
    app.apply_event(&AgentEvent::ToolFinished {
        index: 0,
        call_id: "c1".into(),
        is_error: false,
    });
    assert_eq!(app.session.note, None, "the tool finished");

    // A batch: the note stays until the last tool of the batch finishes.
    for (index, (call_id, name)) in [("c2", "shell"), ("c3", "read")].into_iter().enumerate() {
        app.apply_event(&AgentEvent::ToolStarted {
            index,
            call_id: call_id.into(),
            name: name.into(),
        });
    }
    app.apply_event(&AgentEvent::ToolFinished {
        index: 0,
        call_id: "c2".into(),
        is_error: false,
    });
    assert!(app.session.note.is_some(), "one tool is still running");
    app.apply_event(&AgentEvent::ToolFinished {
        index: 1,
        call_id: "c3".into(),
        is_error: false,
    });
    assert_eq!(app.session.note, None, "the last tool finished");

    // The next model request starts with the note cleared.
    app.apply_event(&AgentEvent::ModelRequestStarted {
        model_call: 1,
        budget: None,
    });
    assert_eq!(app.session.note, None);
}

/// The note must name a tool that is **still running**. `ToolFinished` is emitted in completion
/// order, "which may differ from the transcript order" (radian-core's own event contract), so the
/// call the note names can settle while another call of the same batch is still running. A note
/// naming a settled tool is the same defect as the stuck "tool shell running".
#[test]
fn the_note_names_a_tool_that_is_still_running() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::ToolStarted {
        index: 0,
        call_id: "c1".into(),
        name: "shell".into(),
    });
    app.apply_event(&AgentEvent::ToolStarted {
        index: 1,
        call_id: "c2".into(),
        name: "read".into(),
    });
    assert_eq!(app.session.note.as_deref(), Some("tool read running"));

    // `read` (the call the note names) settles first; `shell` is still running.
    app.apply_event(&AgentEvent::ToolFinished {
        index: 1,
        call_id: "c2".into(),
        is_error: false,
    });
    assert_eq!(
        app.session.note.as_deref(),
        Some("tool shell running"),
        "the settled call must not stay named"
    );
    // The footer itself (the default preset's chrome line), not only the field behind it.
    let terminal = render(&mut app, 80, 12);
    let text = screen(&terminal);
    assert!(text.contains("tool shell running"), "{text}");
    assert!(!text.contains("tool read running"), "{text}");

    // When the last call settles the note goes, before the next model request.
    app.apply_event(&AgentEvent::ToolFinished {
        index: 0,
        call_id: "c1".into(),
        is_error: false,
    });
    assert_eq!(app.session.note, None);
}

/// A tool note may not outlive its batch: the next model request starts only after every call of
/// the batch settled, so a `ToolStarted` whose `ToolFinished` never came must be forgotten there.
/// The core does produce that: when a journal append fails, `run_group` discards the results of
/// the calls already running (`radian-core/src/turn.rs`, the `journal_failed` path), so no
/// `ToolFinished` is emitted for them.
#[test]
fn the_tool_note_does_not_survive_the_next_model_request() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::ToolStarted {
        index: 0,
        call_id: "c1".into(),
        name: "shell".into(),
    });
    assert_eq!(app.session.note.as_deref(), Some("tool shell running"));

    // Its `ToolFinished` was lost; the turn moved on to the next model request.
    app.apply_event(&AgentEvent::ModelRequestStarted {
        model_call: 2,
        budget: None,
    });
    assert_eq!(
        app.session.note, None,
        "the next request ends the tool phase"
    );
}

#[test]
fn the_status_line_shows_model_and_turn_state() {
    let mut app = App::new("my-model", "s");
    classic_footer(&mut app);
    // The turn state is a session fact: the event sink writes it to the projection
    // (ADR 0023 U5), so the test writes it there too.
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    app.session.entries.push(Entry::new(EntryKind::User, "hi"));
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(text.contains("my-model"), "{text}");
    assert!(text.contains("running"), "{text}");
}

/// ADR 0027 + ADR 0035's 2026-10-16 note: the `box` frame's top rule carries the
/// running status where a code block's language label sits — `╭─ <spinner> Working · 12s ·
/// esc to interrupt ───…` — and is a plain full-width rule when idle. The `lines` style keeps
/// revision 2's status on its top rule.
#[test]
fn the_input_rule_shows_the_spinner_status_while_running() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Box;
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    // The frame is a function of the injected clock (ADR 0027).
    app.set_now(app.session.spinner_origin);
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(
        text.contains("╭─ ◜ Working"),
        "the spinner in the box's top border: {text}"
    );

    // Idle: no spinner, no text; the top rule has no label.
    app.session.projection.set_turn(radian_tui::TurnState::Idle);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(!text.contains("Working"), "an idle rule is plain: {text}");
    assert!(!text.contains('◜'), "{text}");
}

/// ADR 0018 + ADR 0027: a running compaction shows `Compacting` on the input's top
/// rule (the `CompactionStarted`/`CompactionFinished` events exist, so this is not invented).
#[test]
fn the_input_rule_shows_compacting() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Box;
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    app.set_now(app.session.spinner_origin);
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    app.session.compacting = true;
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(text.contains("◜ Compacting"), "{text}");

    // The flag clears when the compaction ends.
    app.session.compacting = false;
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(!text.contains("Compacting"), "{text}");
}

/// ADR 0035's 2026-10-16 note: while a turn runs, the `box` frame's top rule carries the status
/// in the **code-block language label** position (`╭─ <spinner> Working · 12s · esc to
/// interrupt ───…`), and it is **not bold** (the pulse takes the spinner role, the words the
/// dim/hint level, ADR 0027).
#[test]
fn the_box_frame_puts_the_status_in_the_label_position_and_never_bold() {
    use ratatui::style::Modifier;
    let mut app = App::new("m", "s");
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    app.set_now(app.session.spinner_origin);
    app.input_style = radian_tui::render::InputStyle::Box;
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    app.session
        .timer
        .request_started(app.session.spinner_origin);
    app.set_now(app.session.spinner_origin + std::time::Duration::from_secs(12));
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    // The label sits after `╭─ ` (the code block's label column) and the rule fills to the end.
    let top = row_of(&terminal, "╭─").expect("the top rule");
    let top_text = row(&terminal, top as u16);
    assert!(
        top_text.starts_with("╭─ ◜ Working · 12s · esc to interrupt ─"),
        "the status in the label position: {top_text:?}"
    );
    // Not bold: neither the spinner nor the words carry BOLD.
    let spinner = cell_style(&terminal, 3, top as u16);
    assert!(
        !spinner.add_modifier.contains(Modifier::BOLD),
        "the spinner is not bold: {spinner:?}"
    );
    let words = cell_style(&terminal, 5, top as u16);
    assert!(
        !words.add_modifier.contains(Modifier::BOLD),
        "the status words are not bold: {words:?}"
    );
    assert!(
        text.contains("╰──"),
        "the frame keeps its three-cell tail: {text}"
    );
}

#[test]
fn live_deltas_are_replaced_by_the_committed_message_not_duplicated() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    let projection = radian_tui::projection::SessionProjection::new();
    stream(&mut app, &projection, StreamKind::Assistant, "hel");
    stream(&mut app, &projection, StreamKind::Assistant, "lo");
    assert_eq!(app.session.entries.len(), 1);
    assert_eq!(app.session.entries[0].text, "hello");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("hello", "", false, vec![]),
    });
    let assistant: Vec<&Entry> = app
        .session
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "the live view is replaced, not kept");
    assert_eq!(assistant[0].text, "hello");
}

/// An `@` file attachment is folded to one line for display; the message itself is unchanged
/// (the journal keeps the full text).
#[test]
fn a_user_file_attachment_is_folded_to_one_line() {
    let mut app = App::new("mock", "s");
    app.session.entries.push(Entry::new(
        EntryKind::User,
        "look\n\n<file path=\"notes.txt\">\nhello\nworld\n</file>",
    ));
    let text = screen(&render(&mut app, 80, 12));
    assert!(text.contains("[file: notes.txt, 2 lines]"), "{text}");
    assert!(
        !text.contains("hello"),
        "the content must not be shown: {text}"
    );
    assert!(text.contains("look"), "{text}");
    assert_eq!(
        app.session.entries[0].text, "look\n\n<file path=\"notes.txt\">\nhello\nworld\n</file>",
        "the entry text is the full message"
    );
}

fn candidate(label: &str) -> radian_tui::command::Candidate {
    radian_tui::command::Candidate {
        insert: label.into(),
        description: String::new(),
        current: false,
    }
}

/// The `@` completion list is drawn above the input box and is clipped to the terminal width.
#[test]
fn the_completion_list_sits_above_the_input_and_truncates() {
    let mut app = App::new("m", "s");
    app.input = "@no".into();
    app.completion_set(vec![candidate("notes.txt"), candidate("notes.md")]);
    let text = screen(&render(&mut app, 40, 12));
    assert!(text.contains("notes.txt"), "{text}");
    assert!(text.contains("notes.md"), "{text}");
    let candidate = text.find("notes.md").expect("the list is shown");
    let input = text.rfind("@no").expect("the input box is shown");
    assert!(
        candidate < input,
        "the list must be above the input:\n{text}"
    );
}

#[test]
fn the_completion_list_truncates_on_a_narrow_terminal() {
    let mut app = App::new("m", "s");
    app.input = "@".into();
    app.completion_set(vec![candidate("a-very-long-candidate-name.txt")]);
    let text = screen(&render(&mut app, 12, 10));
    assert!(
        !text.contains("a-very-long-candidate-name.txt"),
        "the row must be clipped, not wrapped:\n{text}"
    );
    // The `❯ ` cursor takes two of the twelve columns.
    assert!(text.contains("a-very-lon"), "{text}");
    assert!(!text.contains("candidate"), "{text}");
}

/// The list cuts a row to the width (the cursor is 2 columns), so the rule is testable
/// without a terminal that clips.
#[test]
fn completion_rows_truncates_to_the_width() {
    let mut app = App::new("m", "s");
    app.completion_set(vec![candidate("a-very-long-name.txt")]);
    let rows = render::completion_rows(&app, 6, 10);
    let text: String = rows[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.starts_with("❯ a-ve"), "{text:?}");
    assert_eq!(unicode_width::UnicodeWidthStr::width(text.as_str()), 6);
}

/// ADR 0035: the menu scrolls instead of a `+N more` line, and the window follows
/// the highlight.
#[test]
fn the_completion_list_scrolls_instead_of_showing_more() {
    let mut app = App::new("m", "s");
    app.completion_set(
        (0..15)
            .map(|i| candidate(&format!("file{i:02}.txt")))
            .collect(),
    );
    let text = screen(&render(&mut app, 40, 20));
    assert!(text.contains("file00.txt"), "{text}");
    assert!(!text.contains("file10.txt"), "{text}");
    assert!(!text.contains("more"), "no +N more line: {text}");
    // Move the highlight past the window: the list scrolls to keep it visible.
    for _ in 0..11 {
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    let text = screen(&render(&mut app, 40, 20));
    assert!(
        text.contains("file11.txt"),
        "the window follows the highlight: {text}"
    );
    assert!(!text.contains("file00.txt"), "{text}");
}

/// A candidate with a description column (the current flag is set by the test).
fn described(label: &str, description: &str) -> radian_tui::command::Candidate {
    radian_tui::command::Candidate {
        insert: label.into(),
        description: description.into(),
        current: false,
    }
}

/// The display column where a row's description starts: right after the label, its optional
/// current mark and the padding. The cursor `❯ ` is a wide glyph, so the prefix is measured by
/// display width, not by byte index.
fn description_column(row: &str, label: &str) -> usize {
    let after_label = row.find(label).expect("the label is on the row") + label.len();
    let rest = &row[after_label..];
    let (marker, rest) = match rest.strip_prefix(" ✓").or_else(|| rest.strip_prefix(" *")) {
        Some(rest) => (2, rest),
        None => (0, rest),
    };
    let spaces = rest.len() - rest.trim_start_matches(' ').len();
    unicode_width::UnicodeWidthStr::width(&row[..after_label]) + marker + spaces
}

/// A completion menu marks the item in effect with ` ✓` right after its name
/// (never in the description), so every row's description starts in the same column. `/theme`,
/// `/model` and `/thinking` all draw through the shared list.
fn assert_menu_marks_current(app: &mut App, labels: &[&str], current: usize) {
    let terminal = render(app, 80, 20);
    let text = screen(&terminal);
    assert!(
        text.contains(&format!("{} ✓", labels[current])),
        "the current row is marked after its name:\n{text}"
    );
    assert!(
        !text.contains("current"),
        "no row carries the old marker text:\n{text}"
    );
    let columns: Vec<usize> = labels
        .iter()
        .map(|label| {
            let y = row_of(&terminal, label).expect("a menu row") as u16;
            description_column(&row(&terminal, y), label)
        })
        .collect();
    assert!(
        columns.windows(2).all(|pair| pair[0] == pair[1]),
        "a row's description column moved: {columns:?}\n{text}"
    );
    // The mark is drawn in the `status_ok` role.
    let mark = style_of(&terminal, "✓").expect("the current mark is on screen");
    assert_eq!(
        mark.fg, app.theme.status_ok.fg,
        "the mark uses the status_ok role:\n{text}"
    );
}

#[test]
fn the_theme_model_and_thinking_menus_mark_the_current_row() {
    // `/theme`: the built-ins, `mono` in effect.
    let mut theme: Vec<radian_tui::command::Candidate> =
        radian_tui::command::theme_candidates(None);
    theme
        .iter_mut()
        .find(|c| c.insert == "mono")
        .unwrap()
        .current = true;
    let mut app = App::new("m", "s");
    app.input = "/theme ".into();
    app.completion_set(theme);
    assert_menu_marks_current(&mut app, &["dark", "light", "mono"], 2);

    // `/model`: the flat list, `opencode-go/deepseek-v4.1-flash` in effect.
    let mut model = vec![
        described("anthropic/claude-4", "anthropic"),
        described("opencode-go/deepseek-v4.1-flash", "opencode-go"),
        described("openai/gpt-5", "openai"),
    ];
    model[1].current = true;
    let mut app = App::new("m", "s");
    app.input = "/model ".into();
    app.completion_set(model);
    assert_menu_marks_current(
        &mut app,
        &[
            "anthropic/claude-4",
            "opencode-go/deepseek-v4.1-flash",
            "openai/gpt-5",
        ],
        1,
    );

    // `/thinking`: the real producer, `high` in effect.
    let opencode =
        radian_host::ProviderRef::BuiltIn(radian_host::descriptor("opencode-go").unwrap());
    let thinking =
        radian_tui::command::thinking_candidates(&opencode, "deepseek-v4.1-flash", 4096, "high");
    let mut app = App::new("m", "s");
    app.input = "/thinking ".into();
    app.completion_set(thinking);
    assert_menu_marks_current(&mut app, &["low", "high", "max"], 1);
}

// ---------- the question form (ADR 0024 Q7) ----------

fn option(id: &str, label: &str) -> radian_coding::QuestionOption {
    radian_coding::QuestionOption {
        id: id.into(),
        label: label.into(),
        description: None,
        recommended: false,
    }
}

fn form(questions: Vec<radian_coding::Question>) -> radian_tui::question::QuestionForm {
    radian_tui::question::QuestionForm::new(7, "session".into(), questions)
}

fn panel_text(form: &radian_tui::question::QuestionForm, width: usize) -> String {
    text_of(&render::question_panel(
        form,
        "session",
        width,
        usize::MAX,
        &radian_tui::theme::Theme::dark(),
        radian_tui::footer::Glyphs::Unicode,
    ))
}

fn text_of(lines: &[ratatui::text::Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A long option list scrolls so the cursor stays visible, and the clipped rows are counted.
#[test]
fn a_long_option_list_scrolls_to_keep_the_cursor_visible() {
    let options: Vec<_> = (0..12)
        .map(|index| option(&format!("o{index}"), &format!("Option {index}")))
        .collect();
    let mut form = form(vec![radian_coding::Question {
        id: "q".into(),
        header: None,
        prompt: "Pick one".into(),
        kind: radian_coding::QuestionKind::Single,
        options,
        allow_other: false,
        required: true,
    }]);
    // The panel lays the whole list out; the frame clips it to the modal height.
    form.answers[0].cursor = 11;
    let shown = panel_text(&form, 40);
    assert!(shown.contains("❯ ○ Option 11"), "{shown}");
    assert!(shown.contains("Option 0"), "{shown}");
}

/// A narrow box clips its rows instead of wrapping them (ADR 0024 Q7).
#[test]
fn a_narrow_question_box_clips_instead_of_wrapping() {
    let form = form(vec![radian_coding::Question {
        header: None,
        id: "q".into(),
        prompt: "a prompt that is far too long for the box".into(),
        kind: radian_coding::QuestionKind::Single,
        options: vec![option("a", "an option label that is also far too long")],
        allow_other: false,
        required: false,
    }]);
    let shown = panel_text(&form, 20);
    for line in shown.lines() {
        assert!(
            line.chars().count() <= 20,
            "a row must be clipped to the width: {line:?}"
        );
    }
    assert!(shown.contains('…'), "the clip is marked: {shown}");
}

/// The overview lists every question with its answer, and says so when one is unanswered.
#[test]
fn the_overview_lists_each_question_and_its_answer() {
    let mut form = form(vec![
        radian_coding::Question {
            header: None,
            id: "one".into(),
            prompt: "Pick one".into(),
            kind: radian_coding::QuestionKind::Single,
            options: vec![option("a", "A")],
            allow_other: false,
            required: false,
        },
        radian_coding::Question {
            header: None,
            id: "two".into(),
            prompt: "Say something".into(),
            kind: radian_coding::QuestionKind::Text,
            options: Vec::new(),
            allow_other: false,
            required: false,
        },
    ]);
    form.current = None;
    form.answers[0].selected = vec!["a".into()];
    let shown = panel_text(&form, 60);
    // The `Submit` tab lists each question by its header and answer.
    assert!(shown.contains("✓ one — A"), "{shown}");
    assert!(shown.contains("· two — (not answered)"), "{shown}");
}

/// The decline confirmation is shown, so the first `Esc` never looks like it did nothing.
#[test]
fn the_decline_confirmation_is_shown() {
    let mut form = form(vec![radian_coding::Question {
        header: None,
        id: "one".into(),
        prompt: "Pick one".into(),
        kind: radian_coding::QuestionKind::Text,
        options: Vec::new(),
        allow_other: false,
        required: false,
    }]);
    form.confirm_decline = true;
    let shown = panel_text(&form, 60);
    assert!(shown.contains("press Esc again to decline"), "{shown}");
}

/// The input area in the style of pi: a rule above and below, `> ` on the
/// first line, and continuation lines indented by two cells instead of repeating the prompt.
/// This is `tui.input_style = "lines"` (the default now, ADR 0035).
#[test]
fn the_input_area_has_rules_and_a_prompt_instead_of_a_box() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Lines;
    app.input = "first\nsecond".into();
    let text = screen(&render(&mut app, 20, 8));
    assert!(!text.contains("message"), "the box title is gone:\n{text}");
    let lines: Vec<&str> = text.lines().collect();
    let rules: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("────"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(rules.len(), 2, "a rule above and below:\n{text}");
    // The two text rows sit between the rules, with the prompt on the first and the
    // continuation indented by the same two cells.
    assert_eq!(lines[rules[0] + 1].trim_end(), "❯ first", "{text}");
    assert_eq!(lines[rules[0] + 2].trim_end(), "  second", "{text}");
    assert_eq!(
        rules[1],
        rules[0] + 3,
        "two text rows between them:\n{text}"
    );
    // No vertical border inside the input area (the status bar's `│` separators are elsewhere).
    for line in &lines[rules[0]..=rules[1]] {
        assert!(!line.contains('│'), "no vertical border:\n{text}");
    }
}

/// The empty input shows the placeholder after the `❯ ` prompt in both styles.
#[test]
fn the_empty_input_shows_the_placeholder_after_the_prompt() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Lines;
    let text = screen(&render(&mut app, 80, 8));
    assert!(
        text.contains("❯ (type a message, Enter to send, Ctrl-J for a new line)"),
        "{text}"
    );

    let mut boxed = App::new("m", "s");
    boxed.input_style = radian_tui::render::InputStyle::Box;
    let text = screen(&render(&mut boxed, 80, 8));
    assert!(
        text.contains("│ ❯ (type a message, Enter to send, Ctrl-J for a new line)"),
        "the box's placeholder follows its rail and prompt:\n{text}"
    );
}

/// `tui.input_style = "box"` is the thin half frame (ADR 0035 / the
/// 2026-10-16 note): the same open frame as a code block. Idle is a full-width top rule with
/// **no label**, each input row `│ ` plus the `❯ ` prompt (`│   ` for a continuation), and a
/// three-cell `╰──` tail.
#[test]
fn the_box_input_style_draws_the_thin_half_frame() {
    let mut app = App::new("m", "s");
    app.input = "first\nsecond".into();
    app.input_style = radian_tui::render::InputStyle::Box;
    let terminal = render(&mut app, 20, 8);
    let text = screen(&terminal);
    assert!(!text.contains("message"), "no label on the frame:\n{text}");
    assert!(
        !text.contains('╮') && !text.contains('┌'),
        "no closed box:\n{text}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let top = lines
        .iter()
        .position(|line| line.starts_with('╭'))
        .unwrap_or_else(|| panic!("no top rule:\n{text}"));
    // The idle top rule is the full input width, `╭` then `─` with no label.
    assert_eq!(
        lines[top].trim_end(),
        format!("╭{}", "─".repeat(19)),
        "the full-width top rule, no label:\n{text}"
    );
    // Each input row: the `│ ` rail, then `❯ ` on the first and two spaces on the continuation.
    assert_eq!(lines[top + 1].trim_end(), "│ ❯ first", "{text}");
    assert_eq!(lines[top + 2].trim_end(), "│   second", "{text}");
    assert_eq!(
        lines[top + 3].trim_end(),
        "╰──",
        "the three-cell tail:\n{text}"
    );
}

/// A CJK input wraps by display width; every wrapped row keeps the `│ ` rail and the text
/// starts in the same column.
#[test]
fn the_box_frame_wraps_a_cjk_line() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Box;
    app.input = "中文中文中文".into();
    let text = screen(&render(&mut app, 12, 8));
    let rows: Vec<&str> = text.lines().collect();
    let top = rows
        .iter()
        .position(|line| line.starts_with('╭'))
        .unwrap_or_else(|| panic!("no top rule:\n{text}"));
    // Inner width 8: four 2-cell characters on the first row, the rest wrapped (a wide glyph's
    // skip cell reads as a space, so only the prefix is asserted).
    assert!(rows[top + 1].starts_with("│ ❯ 中"), "{text:?}");
    assert!(
        rows[top + 2].starts_with("│   中"),
        "the wrapped tail keeps the rail and the text column:\n{text:?}"
    );
    assert_eq!(rows[top + 3].trim_end(), "╰──", "{text:?}");
}

// ---------- the `box` input's thin half frame (ADR 0035's 2026-10-16 note) ----------

/// Row `y` as a string, trailing cells kept so the frame's width is visible.
fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.left()..area.right())
        .map(|x| {
            buffer
                .cell((x, y))
                .map(|c| c.symbol())
                .unwrap_or(" ")
                .to_string()
        })
        .collect()
}

/// The index of the first screen row whose text contains `needle`.
fn row_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<usize> {
    let area = *terminal.backend().buffer().area();
    (area.top()..area.bottom())
        .find(|y| row(terminal, *y).contains(needle))
        .map(usize::from)
}

/// The style of the cell at `(x, y)`.
fn cell_style(terminal: &Terminal<TestBackend>, x: u16, y: u16) -> ratatui::style::Style {
    terminal
        .backend()
        .buffer()
        .cell((x, y))
        .map(|cell| cell.style())
        .expect("a cell inside the buffer")
}

/// `tui.footer.glyphs = "ascii"` reaches the box frame's own glyphs (ADR 0027: the
/// unicode glyphs are only used when the font set has them; ADR 0035 maps
/// the code frame's `╭─`/`╰──` to `+-`/`+--`).
#[test]
fn the_box_frame_falls_back_to_ascii() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Box;
    app.input = "hi".into();
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        ..Default::default()
    });
    app.set_now(app.session.spinner_origin);
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let text = screen(&render(&mut app, 60, 10));
    assert!(
        text.contains("+- * Working · esc to interrupt"),
        "the ascii frame and pulse in the label position:\n{text}"
    );
    assert!(text.contains("| ❯ hi"), "the ascii rail:\n{text}");
    assert!(
        !text.contains('╭') && !text.contains('╰'),
        "no unicode frame under ascii:\n{text}"
    );
}

/// A composer attachment with only the facts its placeholder reads.
fn attachment(name: &str, width: u32, height: u32) -> radian_coding::LoadedImage {
    radian_coding::LoadedImage {
        name: name.to_string(),
        mime: "image/png",
        width,
        height,
        bytes: 1024,
        data: String::new(),
    }
}

/// The display column of `needle`'s first byte in `text` (a box rail is one cell but three
/// bytes, so a byte offset is not a column).
fn display_column(text: &str, needle: char) -> usize {
    unicode_width::UnicodeWidthStr::width(&text[..text.find(needle).expect("the needle")])
}

/// ADR 0028 R5: a composer attachment row is a composer row like the text rows. Its placeholder
/// starts in the text column — after the prompt, and inside the box's frame — with one and with
/// two attachments, in both input styles.
#[test]
fn a_composer_attachment_starts_in_the_text_column() {
    use radian_tui::render::InputStyle;
    for style in [InputStyle::Lines, InputStyle::Box] {
        for count in [1, 2] {
            let mut app = App::new("m", "s");
            app.input_style = style;
            app.input = "hi".into();
            app.composer_images = (1..=count)
                .map(|n| attachment(&format!("shot{n}.png"), 10, 10))
                .collect();
            let terminal = render(&mut app, 60, 12);
            let text = screen(&terminal);
            let text_row = row_of(&terminal, "❯ hi").expect("the text row");
            let text_col = display_column(&row(&terminal, text_row as u16), 'h');
            let image_rows: Vec<u16> = (0..12)
                .filter(|y| row(&terminal, *y).contains("[image:"))
                .collect();
            assert_eq!(image_rows.len(), count, "{style:?}:\n{text}");
            for y in image_rows {
                let image_row = row(&terminal, y);
                assert_eq!(
                    display_column(&image_row, '['),
                    text_col,
                    "{style:?}: the attachment row starts in the text column:\n{text}"
                );
                if style == InputStyle::Box {
                    assert_eq!(
                        image_row.chars().next(),
                        Some('│'),
                        "the frame's rail on every attachment row:\n{text}"
                    );
                }
            }
        }
    }
}

/// ADR 0028 R5: a placeholder wider than the inner width is cut with `…` at the inner width. It
/// never wraps and never pushes the frame: the chrome above and below it keeps its row.
#[test]
fn a_composer_attachment_wider_than_the_input_is_cut_with_an_ellipsis() {
    use radian_tui::render::InputStyle;
    let name = "arc-one-task-from-request-to-merge.webp";
    for style in [InputStyle::Lines, InputStyle::Box] {
        let mut app = App::new("m", "s");
        app.input_style = style;
        app.input = "hi".into();
        app.composer_images.push(attachment(name, 3432, 3198));
        let terminal = render(&mut app, 40, 12);
        let text = screen(&terminal);
        let image_rows: Vec<u16> = (0..12)
            .filter(|y| row(&terminal, *y).contains("[image:"))
            .collect();
        assert_eq!(image_rows.len(), 1, "the row never wraps:\n{text}");
        let y = image_rows[0];
        let image_row = row(&terminal, y);
        assert!(
            image_row.trim_end().ends_with('…'),
            "cut with an ellipsis:\n{text}"
        );
        assert!(
            !image_row.contains(name),
            "the full placeholder is gone:\n{text}"
        );
        // The row fills the input area and no more: the last cell of the row is still inside it.
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(image_row.as_str()),
            40,
            "the cut row keeps the frame's width:\n{text}"
        );
        assert_eq!(
            display_column(&image_row, '…'),
            39,
            "the ellipsis sits in the last cell:\n{text}"
        );
        // The chrome above and below the attachment row keeps its own row: the top rule (or the
        // box's full-width top) directly above, the text row and then the bottom chrome after.
        let above = row(&terminal, y - 1);
        let below = row(&terminal, y + 2);
        match style {
            InputStyle::Lines => {
                assert!(above.starts_with("────"), "the top rule:\n{text}");
                assert!(below.starts_with("────"), "the bottom rule:\n{text}");
            }
            InputStyle::Box => {
                assert!(above.starts_with('╭'), "the box's top:\n{text}");
                assert_eq!(
                    unicode_width::UnicodeWidthStr::width(above.as_str()),
                    40,
                    "the top rule stays full width:\n{text}"
                );
                assert_eq!(below.trim_end(), "╰──", "the box's tail:\n{text}");
            }
        }
    }
}

/// A file name may contain a line break. The attachment row is one row of composer chrome
/// (ADR 0028 R5): the placeholder normalizes the control character, so it stays on that row,
/// in the text column, and the frame's chrome keeps its rows.
#[test]
fn a_composer_attachment_name_with_a_line_break_stays_on_one_row() {
    use radian_tui::render::InputStyle;
    for style in [InputStyle::Lines, InputStyle::Box] {
        let mut app = App::new("m", "s");
        app.input_style = style;
        app.input = "hi".into();
        app.composer_images
            .push(attachment("bad\nname\r.png", 10, 10));
        let terminal = render(&mut app, 60, 12);
        let text = screen(&terminal);
        let image_rows: Vec<u16> = (0..12)
            .filter(|y| row(&terminal, *y).contains("[image:"))
            .collect();
        assert_eq!(
            image_rows.len(),
            1,
            "{style:?}: one attachment row, as reserved:\n{text:?}"
        );
        let image_row = row(&terminal, image_rows[0]);
        assert!(
            image_row.contains("[image: bad name .png, 10×10, 1 KB]"),
            "{style:?}: the control characters became spaces:\n{text:?}"
        );
        assert!(
            !image_row.chars().any(char::is_control),
            "{style:?}: no control character reaches the row:\n{text:?}"
        );
        // The text column and the chrome hold exactly as for a plain name.
        let text_row = row_of(&terminal, "❯ hi").expect("the text row");
        assert_eq!(
            display_column(&image_row, '['),
            display_column(&row(&terminal, text_row as u16), 'h'),
            "{style:?}: the attachment row starts in the text column:\n{text:?}"
        );
        match style {
            InputStyle::Lines => assert!(
                row(&terminal, text_row as u16 + 1).starts_with("────"),
                "the bottom rule keeps its row:\n{text:?}"
            ),
            InputStyle::Box => {
                assert_eq!(
                    image_row.chars().next(),
                    Some('│'),
                    "the frame's rail on the attachment row:\n{text:?}"
                );
                assert!(
                    row(&terminal, text_row as u16 + 1).trim_end() == "╰──",
                    "the frame's tail keeps its row:\n{text:?}"
                );
            }
        }
    }
}

/// ADR 0035: no tier draws a
/// user half-cell pad. The `ascii` tier draws its own `> ` marker, and two user rows are
/// separated by the container's one blank line like any two blocks.
#[test]
fn the_ascii_tier_has_no_half_cell_pads() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        ..Default::default()
    });
    app.push_message(&Message::user("hello"));
    app.push_message(&Message::user("again"));
    let terminal = render(&mut app, 40, 10);
    let text = screen(&terminal);
    assert!(!text.contains('▄') && !text.contains('▀'), "{text}");
    assert_eq!(row_of(&terminal, "> hello"), Some(0), "{text}");
    assert_eq!(row(&terminal, 1).trim_end(), "", "one blank line: {text}");
    assert_eq!(row_of(&terminal, "> again"), Some(2), "{text}");
}

// ---------- the thin user row (ADR 0035) ----------

/// ADR 0035: the user row is `❯ 文本` in the `user` role, the text
/// from column 3, with no background band, no half-cell padding and no rail. Two user rows sit
/// on consecutive lines (no pad rows between them).
#[test]
fn the_user_row_is_a_chevron_and_the_words_with_no_band() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("hello"));
    app.push_message(&Message::user("中文消息"));
    let terminal = render(&mut app, 40, 10);
    let text = screen(&terminal);
    // One blank line between the two rows (ADR 0035: the user row is an ordinary
    // block now, not its own pad).
    assert!(row(&terminal, 0).starts_with("❯ hello"), "{text:?}");
    assert_eq!(
        row(&terminal, 1).trim_end(),
        "",
        "the container gap: {text:?}"
    );
    assert!(row(&terminal, 2).starts_with("❯ 中"), "{text:?}");
    // `❯` and the words are the `user` role's foreground; there is no background band.
    let chevron = cell_style(&terminal, 0, 0);
    assert_eq!(chevron.fg, app.theme.user.fg, "{chevron:?}");
    assert_eq!(chevron.bg, Some(Color::Reset), "no band: {chevron:?}");
    let body = cell_style(&terminal, 2, 0);
    assert_eq!(body.fg, app.theme.user.fg, "the words too: {body:?}");
    assert_eq!(body.bg, Some(Color::Reset), "no band: {body:?}");
    // The CJK row's first wide glyph is at column 3 and takes the `user` role.
    assert_eq!(
        cell_style(&terminal, 2, 2).fg,
        app.theme.user.fg,
        "{text:?}"
    );
}

/// ADR 0035: the `ascii` tier draws the user marker as `> `.
#[test]
fn the_user_row_marker_falls_back_to_ascii() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        ..Default::default()
    });
    app.push_message(&Message::user("hello"));
    let terminal = render(&mut app, 40, 8);
    let user_row = row(&terminal, 0);
    assert!(user_row.starts_with("> hello"), "{user_row:?}");
    assert!(!user_row.contains('❯'), "the ascii marker: {user_row:?}");
}

/// The marker column is two cells: a wrapped long user message keeps every continuation row at
/// column 3 (the marker column is blank, never a repeat of `❯ `).
#[test]
fn a_wrapped_user_message_starts_every_row_at_column_three() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("aaaa bbbb cccc dddd"));
    let terminal = render(&mut app, 12, 10);
    let first = row_of(&terminal, "❯ aaaa").expect("the first row");
    let second = row(&terminal, (first + 1) as u16);
    assert!(
        second.starts_with("  cccc"),
        "the continuation row starts at column 3: {second:?}"
    );
    assert!(
        !second.contains('❯'),
        "no marker on the continuation: {second:?}"
    );
}

/// A selection maps screen columns through the two-cell marker and copies only the words
/// (ADR 0035): the `❯ ` marker is drawn, never body text.
#[test]
fn a_user_selection_never_copies_the_marker() {
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("hello"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.session.entries,
        EXPANDED,
        40,
        24,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    // The whole row, from the marker cell (column 0) through the last word.
    let selection = Selection {
        anchor: locate(&app.session.entries, EXPANDED, 40, 24, 0, 0).expect("anchor"),
        head: locate(&app.session.entries, EXPANDED, 40, 24, 0, 20).expect("head"),
    };
    let text = radian_tui::select::logical_text(&app.session.entries, &selection);
    assert_eq!(text, "hello", "the marker is not copied");
}

/// ADR 0035: with no config the compiled default is `lines` (a rule above
/// and below with the `❯ ` prompt); `bar` was removed by the 2026-10-16 note.
#[test]
fn the_default_input_style_is_lines() {
    let mut app = App::new("m", "s");
    assert_eq!(
        app.input_style,
        radian_tui::render::InputStyle::Lines,
        "the compiled default"
    );
    app.input = "hello".into();
    let text = screen(&render(&mut app, 30, 8));
    assert!(text.contains("❯ hello"), "{text}");
    assert!(
        text.lines().any(|line| line.starts_with("────")),
        "the `lines` rule: {text}"
    );
    assert!(!text.contains('▌'), "no railway band: {text}");
}

/// ADR 0035 / ADR 0028: the folded compaction line
/// starts at column 1 in the `frame` role (it used to be indented to the body column).
#[test]
fn the_compaction_line_starts_at_column_one_in_the_frame_role() {
    let history = radian_core::compaction::compacted_history("the earlier work", &[], &[])
        .expect("a valid history");
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&history[0]);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    let y =
        u16::try_from(row_of(&terminal, "── context compacted").expect("the line")).expect("a row");
    let line = row(&terminal, y);
    assert!(
        line.starts_with("── context compacted ──"),
        "column 1: {line:?}"
    );
    assert_eq!(
        cell_style(&terminal, 0, y).fg,
        app.theme.frame.fg,
        "the `frame` role: {text}"
    );
}

/// ADR 0035: thinking has **one blank line on both
/// sides** — folded or expanded, and whether the block below it is the reply or a tool card. The
/// gap above it stays one line (the container's gap after the user row).
#[test]
fn thinking_has_one_blank_line_on_both_sides() {
    // Folded, before the reply.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("q"));
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("the answer", "a thought", false, vec![]),
    });
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let user = row_of(&terminal, "❯ q").expect("the user text");
    let thinking = row_of(&terminal, "thinking · 1 lines").expect("the folded channel");
    let answer = row_of(&terminal, "the answer").expect("the reply");
    assert_eq!(
        thinking,
        user + 2,
        "the container's one blank line is the gap above thinking:\n{text}"
    );
    assert_eq!(
        answer,
        thinking + 2,
        "one blank line between thinking and the reply:\n{text}"
    );
    assert_eq!(
        row(&terminal, (thinking + 1) as u16).trim_end(),
        "",
        "and that line is empty:\n{text}"
    );

    // Expanded, and directly against a tool card: the same single line. The real flow gives one
    // line too: `push_message` still pushes the (empty) assistant entry between them, but the
    // layout skips a committed text-less reply (ADR 0028 rule 1), so it does not
    // add a second blank row.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.push_message(&assistant_with(
        "",
        "first thought\nsecond thought",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "make test"}),
        }],
    ));
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let first = row_of(&terminal, "first thought").expect("the expanded thinking body");
    let body = row_of(&terminal, "second thought").expect("the second thinking line");
    assert_eq!(
        body,
        first + 1,
        "the body's own lines are adjacent:\n{text}"
    );
    // The tool block starts at its head row (`• `, a running call draws no body) right after
    // the one blank line.
    let head = row_of(&terminal, "• shell").expect("the block's head row");
    assert_eq!(
        head,
        body + 2,
        "one blank line between expanded thinking and the tool block:\n{text}"
    );
    assert_eq!(
        row(&terminal, (body + 1) as u16).trim_end(),
        "",
        "and that line is empty:\n{text}"
    );
}

/// Pushes the result of the call `c1` (`shell $ true`, exit 0) through the real path, so the
/// call/result pair is formed the way the loop forms it.
fn push_c1_result(app: &mut App) {
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("ok")],
            structured: Some(serde_json::json!({"command": "true", "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
}

/// A text-less assistant reply (only reasoning, or nothing at all) is still pushed as its own
/// committed entry (`push_message` keeps it), but the layout skips it: a committed empty reply
/// contributes no line and no blank row of its own, so the block below it is one blank row
/// under the block above it (ADR 0028 rule 1, ADR 0035).
#[test]
fn a_text_less_assistant_reply_leaves_one_blank_row() {
    // Reasoning and a tool call, no reply text, then the result.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.push_message(&Message::user("q"));
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with(
            "",
            "a thought",
            false,
            vec![ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "true"}),
            }],
        ),
    });
    push_c1_result(&mut app);
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let thought = row_of(&terminal, "a thought").expect("the thinking body");
    let head = row_of(&terminal, "◻ shell").expect("the tool head");
    assert_eq!(
        head,
        thought + 2,
        "one blank row between the thinking line and the tool head:\n{text}"
    );
    assert_eq!(
        row(&terminal, (thought + 1) as u16).trim_end(),
        "",
        "and it is empty:\n{text}"
    );

    // No reasoning at all: the user row and the tool head are one blank row apart too.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("q"));
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "true"}),
        }],
    ));
    push_c1_result(&mut app);
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let user = row_of(&terminal, "❯ q").expect("the user row");
    let head = row_of(&terminal, "◻ shell").expect("the tool head");
    assert_eq!(
        head,
        user + 2,
        "one blank row between the user row and the tool head:\n{text}"
    );
    assert_eq!(
        row(&terminal, (user + 1) as u16).trim_end(),
        "",
        "and it is empty:\n{text}"
    );
}

/// A **live** text-less assistant entry (a reply whose first delta has not produced text yet)
/// keeps today's geometry: its block still reserves the container's blank line, so nothing
/// shifts while the reply is starting. Only a committed empty reply is skipped.
#[test]
fn a_live_empty_assistant_keeps_its_blank_line() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    let projection = radian_tui::projection::SessionProjection::new();
    app.push_message(&Message::user("q"));
    stream(&mut app, &projection, StreamKind::Reasoning, "a thought");
    stream(&mut app, &projection, StreamKind::Assistant, "");
    let live = app
        .session
        .entries
        .iter()
        .position(|entry| entry.kind == EntryKind::Assistant && entry.live)
        .expect("the live reply entry");
    assert_eq!(
        radian_tui::blocks::gap_at(&app.session.entries, live),
        1,
        "a live empty entry still reserves the container's blank line"
    );
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let thought = row_of(&terminal, "a thought").expect("the thinking body");
    assert_eq!(
        row(&terminal, (thought + 1) as u16).trim_end(),
        "",
        "the live entry's blank row is still there:\n{text}"
    );
}

/// The gap consumers (the selection mapping and the find geometry) see the skipped reply the
/// same way the drawing does: the tool call's transcript line is one blank row below the
/// reasoning block, and a press on the tool head maps to the call, not to the reply that is
/// not drawn.
#[test]
fn a_skipped_reply_does_not_shift_the_selection_or_find_rows() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.push_message(&Message::user("q"));
    app.push_message(&assistant_with(
        "",
        "a thought",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "true"}),
        }],
    ));
    push_c1_result(&mut app);
    let modes = EXPANDED;
    let width = 40;
    let reasoning = app
        .session
        .entries
        .iter()
        .position(|entry| entry.kind == EntryKind::Reasoning)
        .expect("the reasoning entry");
    let call = app
        .session
        .entries
        .iter()
        .position(|entry| entry.kind == EntryKind::ToolCall)
        .expect("the tool call entry");
    let reasoning_lines =
        radian_tui::render::display_lines(&app.session.entries[reasoning], modes, width, 24).len();
    let (_, starts) = radian_tui::find::compute(&app.session.entries, modes, width, 24, "");
    assert_eq!(
        starts[call] - starts[reasoning],
        reasoning_lines + 1,
        "the tool call starts one blank row (not two) below the reasoning block"
    );
    let hit =
        radian_tui::select::locate_hit(&app.session.entries, modes, width, 24, starts[call], 2)
            .expect("a hit on the tool head");
    assert_eq!(
        hit.anchor.entry, call,
        "the head row maps to the tool call, not to the skipped reply"
    );
}

/// ADR 0028: the `unicode` tier's tool head ends with `✓` when the call
/// succeeded (not `+`), and the `ascii` tier with `+`.
#[test]
fn the_tool_head_ends_with_the_tiers_status_glyph() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({"command": "true"}),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("ok")],
            structured: Some(serde_json::json!({"command": "true", "exit_code": 0})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let text = screen(&render(&mut app, 60, 12));
    assert!(
        text.contains("◻ shell $ true · exit 0 ✓"),
        "a completed block ends its head with the check mark:\n{text}"
    );
    assert!(!text.contains("+ shell"), "never the old `+`:\n{text}");
    assert!(!text.contains("shell: true"), "never the old head:\n{text}");
}

/// A transcript selection draws with the theme's `selection` role: the band lands exactly on
/// the cells `select::highlight_rows` reports, prefix cells included, and the text keeps its
/// own foreground (the patch only adds the background).
#[test]
fn a_selection_draws_the_selection_role_on_its_cells() {
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.session.entries,
        EXPANDED,
        40,
        24,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    // Select "selection" on the block's only line (the thin user row, ADR 0035):
    // body columns 6..15 → screen columns 8..17 (the `❯ ` marker takes the first two cells).
    let selection = Selection {
        anchor: locate(&app.session.entries, EXPANDED, 40, 24, 0, 8).expect("anchor"),
        head: locate(&app.session.entries, EXPANDED, 40, 24, 0, 17).expect("head"),
    };
    app.session.selection = Some(selection);
    // Tall enough that the one-row user block is on screen.
    let terminal = render(&mut app, 40, 10);
    let text = screen(&terminal);
    assert!(text.contains("hello selection"), "{text}");
    let word = style_of(&terminal, "selection").expect("the selected word is on screen");
    assert_eq!(
        word.bg,
        Some(Color::Rgb(50, 84, 103)),
        "the dark theme's selection band"
    );
    let before = style_of(&terminal, "hello").expect("the unselected head of the line");
    assert_eq!(
        before.bg,
        Some(Color::Reset),
        "the user block has no band now (ADR 0029), only the selection is banded"
    );
    assert_eq!(before.fg, word.fg, "the text keeps its own foreground");
}

/// The `mono` theme's selection is reverse video, never a color (`NO_COLOR` and `mono`).
#[test]
fn the_mono_selection_is_reversed_not_colored() {
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.theme = radian_tui::theme::Theme::mono();
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.session.entries,
        EXPANDED,
        40,
        24,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    app.session.selection = Some(Selection {
        anchor: locate(&app.session.entries, EXPANDED, 40, 24, 0, 8).expect("anchor"),
        head: locate(&app.session.entries, EXPANDED, 40, 24, 0, 17).expect("head"),
    });
    let terminal = render(&mut app, 40, 10);
    let word = style_of(&terminal, "selection").expect("on screen");
    assert_eq!(word.bg, Some(Color::Reset), "no colored band in mono");
    assert!(
        word.add_modifier
            .contains(ratatui::style::Modifier::REVERSED)
    );
}

/// A single click's empty selection draws nothing anywhere.
#[test]
fn an_empty_selection_draws_nothing() {
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.session.entries,
        EXPANDED,
        40,
        24,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    let at = locate(&app.session.entries, EXPANDED, 40, 24, 1, 8).expect("anchor");
    app.session.selection = Some(Selection {
        anchor: at,
        head: at,
    });
    let text = screen(&render(&mut app, 40, 10));
    assert!(text.contains("hello selection"), "{text}");
    assert_eq!(
        style_of(&render(&mut app, 40, 10), "selection").map(|s| s.bg),
        Some(Some(Color::Reset)),
        "no selection and no user band now (ADR 0029): the text is unbanded"
    );
}

/// `wrap` (the renderer's hot path) and `wrap_char_offsets` (what selection maps cells
/// through) are two views of one algorithm. This corpus — spaces that collapse, CJK, empty
/// and blank lines, overlong words — pins them together: every line's offsets rebuild the
/// exact string `wrap` produced, and the end gaps stay in bounds. Change either function
/// without the other and this fails.
#[test]
fn wrap_and_wrap_char_offsets_agree() {
    let corpus = [
        ("hello world", 5usize),
        ("hello world", 11),
        ("hello world", 3),
        ("", 5),
        ("\n", 5),
        ("a\n\nb", 5),
        ("  leading and trailing  ", 7),
        ("日本語のテキストは幅で折る", 5),
        ("ab   cd", 4),
        ("ab   cd", 2),
        ("one two three four five six seven", 7),
        ("mixed 日本語 text wrap", 6),
        ("a", 1),
        ("日本", 1),
        ("trailing spaces   ", 5),
        ("   ", 5),
        ("word\n", 5),
        ("\nword", 5),
        ("aaaaaaaaaaaaaaaa", 5),
        ("a b c d e f", 1),
        ("日本 テキスト edge", 3),
        ("combining a\u{301}b", 2),
        // A tab is four columns (ADR 0028 R9) in every view of the wrap.
        ("a\tb", 4),
        ("a\tb", 5),
        ("x\ty\tz", 6),
    ];
    for (text, width) in corpus {
        let lines = radian_tui::render::wrap(text, width);
        let offsets = radian_tui::render::wrap_char_offsets(text, width);
        assert_eq!(
            lines.len(),
            offsets.len(),
            "line counts diverge for {text:?} at {width}"
        );
        for (line, wrapped) in lines.iter().zip(&offsets) {
            let rebuilt: String = wrapped
                .chars
                .iter()
                .map(|&i| {
                    assert!(text.is_char_boundary(i), "offset {i} not a boundary");
                    text[i..].chars().next().unwrap()
                })
                .collect();
            assert_eq!(&rebuilt, line, "diverged for {text:?} at {width}");
            assert!(wrapped.end <= text.len(), "end out of bounds");
        }
    }
}

/// A fenced code block never wraps (ADR 0028 R9): a 200-column line on an 80-column screen
/// is one line ending in an ellipsis, and the next source line is the next screen line.
#[test]
fn a_code_block_does_not_wrap_and_truncates_with_an_ellipsis() {
    let mut app = App::new("m", "s");
    let long = "x".repeat(200);
    app.push_message(&Message::assistant(format!(
        "before\n```rust\n{long}\nsecond\n```\nafter"
    )));
    // 16 rows: the bar input's half-cell pads (ADR 0035) take two rows from the
    // transcript, and the prose above and below the block must stay on screen.
    let terminal = render(&mut app, 80, 16);
    let buffer = terminal.backend().buffer();
    // Find the long line's row: the rail, then 77 'x' and '…' filling the rest of the row.
    let mut found = None;
    for row in 0..16u16 {
        let text: String = (0..80u16)
            .map(|col| {
                buffer
                    .cell((col, row))
                    .map(|c| c.symbol())
                    .unwrap_or(" ")
                    .to_string()
            })
            .collect();
        if text.starts_with("│ ") && text.contains('x') {
            found = Some((row, text));
            break;
        }
    }
    let (row, text) = found.expect("the long code line is on screen");
    assert_eq!(text.chars().filter(|&c| c == 'x').count(), 77, "{text:?}");
    assert!(text.ends_with('…'), "the truncation marker: {text:?}");
    // The next source line is its own row (no soft wrap pushed it down).
    let next: String = (0..80u16)
        .map(|col| {
            buffer
                .cell((col, row + 1))
                .map(|c| c.symbol())
                .unwrap_or(" ")
                .to_string()
        })
        .collect();
    assert!(
        next.starts_with("│ second"),
        "no wrap between code lines: {next:?}"
    );
    assert!(screen(&terminal).contains("before"), "the prose before it");
    assert!(screen(&terminal).contains("after"), "the prose after it");
}

/// Only a notice's **first** line is a structured label (ADR 0028): a wrapped continuation
/// that happens to contain a colon (`12:30`, `https://…`, `path:line`) is prose and keeps the
/// body's level, not the accent.
#[test]
fn a_wrapped_notice_continuation_is_not_a_label() {
    let mut app = App::new("m", "s");
    // Width 27 → body 25, so the break lands just before `12:30`: the continuation starts
    // with a colon token (the case that used to be mis-drawn as an accent label).
    app.push_notice("aaaa bbbb cccc dddd eeee 12:30 ffff");
    let terminal = render(&mut app, 27, 10);
    let text = screen(&terminal);
    assert!(text.contains("12:30"), "{text}");
    let continuation = style_of(&terminal, "12:30").expect("the continuation is on screen");
    assert_eq!(
        continuation.fg, app.theme.notice_body.fg,
        "a continuation is body text, not a label"
    );
    assert_ne!(
        continuation.fg, app.theme.notice_label.fg,
        "it must not get the accent"
    );
    // The first line, which has no colon prefix, is unaffected either way; a first line
    // WITH a label still takes the accent.
    app.push_notice("profile: workspace");
    let terminal = render(&mut app, 27, 10);
    let label = style_of(&terminal, "profile:").expect("the label");
    assert_eq!(label.fg, app.theme.notice_label.fg);
}

/// Tool cards (ADR 0028 R4): the live commit path and the resume replay path are the same
/// `push_message`, so both produce the same header for the same call.
#[test]
fn a_tool_card_head_is_the_same_on_the_live_and_resume_paths() {
    fn call() -> Message {
        Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "make test"}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        }
    }
    fn result() -> Message {
        Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text("ok")],
                structured: Some(serde_json::json!({"command": "make test", "exit_code": 0})),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        }
    }

    // live: committed messages arrive as events.
    let mut live = App::new("m", "s");
    live.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: call(),
    });
    live.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: result(),
    });
    // resume: the journal's messages are replayed through `push_message`.
    let mut resumed = App::new("m", "s");
    resumed.push_message(&call());
    resumed.push_message(&result());

    let live_text = screen(&render(&mut live, 60, 12));
    let resume_text = screen(&render(&mut resumed, 60, 12));
    // The block's head is one line (ADR 0028): `• shell $ make test`
    // with the meta and the status at its end, and the output in `│ ` rows.
    assert!(
        live_text.contains("◻ shell $ make test · exit 0 ✓"),
        "{live_text}"
    );
    assert!(live_text.contains("│ ok"), "the output body: {live_text}");
    assert!(
        !live_text.contains("┌") && !live_text.contains("├──"),
        "no box, no divider: {live_text}"
    );
    assert_eq!(live_text, resume_text, "live and resume render identically");
}

/// The tool fold (ADR 0028 R4): the three modes change the transcript's line count, Ctrl-O
/// steps through them, and the count is never stale (the fold is a layout cache key).
#[test]
fn the_tool_fold_changes_the_line_count_and_ctrl_o_walks_it() {
    use radian_tui::tools::ToolFold;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "generic_tool".into(),
                arguments: serde_json::json!({}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
    });
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(body)],
                structured: None,
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        },
    });
    // errors (the default): a success folds to the last five rows.
    assert_eq!(app.tool_display.fold, ToolFold::Errors);
    let _ = render(&mut app, 60, 30);
    let folded = app.session.total_lines;
    assert!(folded < 12, "folded: {folded} lines");
    // Ctrl-O: errors -> expanded -> collapsed -> errors.
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Expanded);
    let _ = render(&mut app, 60, 30);
    let expanded = app.session.total_lines;
    assert_eq!(
        expanded,
        folded + 15,
        "20 rows instead of the 5 preview rows"
    );
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Collapsed);
    let terminal = render(&mut app, 60, 30);
    // `collapsed` keeps the head only (ADR 0028): the five preview rows
    // `errors` showed are gone, and the meta counts them (`· 20 lines`).
    assert_eq!(
        app.session.total_lines,
        folded - 5,
        "collapsed shows the head only"
    );
    let text = screen(&terminal);
    assert!(text.contains("· 20 lines ✓"), "{text}");
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Errors);
}

/// A unified diff body colors its rows with the theme's own roles (ADR 0028 decision
/// 5 + ADR 0029): added, removed, context, and the hunk/file-header structure — whose rows now
/// carry a line number and a synthesized space after the sign.
#[test]
fn a_patch_body_colors_its_line_kinds() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: serde_json::json!({"path": "x.rs"}),
        }],
    ));
    let diff = "diff --git a/x.rs b/x.rs\nindex 1..2 100644\n--- a/x.rs\n+++ b/x.rs\n@@ -1,2 +1,2 @@\n context\n-old line\n+new line";
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text(diff)],
            structured: None,
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    // Expanded, so the whole diff shows.
    app.tool_display.fold = radian_tui::tools::ToolFold::Expanded;
    let terminal = render(&mut app, 70, 20);
    let theme = app.theme;
    let style = |needle: &str| style_of(&terminal, needle).expect(needle);
    assert_eq!(style("+ new line").fg, theme.diff_added.fg, "added");
    assert_eq!(style("- old line").fg, theme.diff_removed.fg, "removed");
    assert_eq!(style("context").fg, theme.diff_context.fg, "context");
    // `@@` and the file header are structure: the weak `frame` role (ADR 0028
    // decision 5), never the accent `diff_hunk` used to carry.
    assert_eq!(style("@@").fg, theme.frame.fg, "@@ is structure");
    assert_eq!(
        style("diff --git").fg,
        theme.frame.fg,
        "the file header is structure"
    );
    assert_ne!(
        style("@@").fg,
        theme.notice_label.fg,
        "@@ is not the accent"
    );
    // The head names the tool and the call's path.
    let text = screen(&terminal);
    assert!(text.contains("◻ patch x.rs"), "{text}");
}

/// A `patch` card shows the replacement: the head's +N/-M and the `-old`/`+new` body, whose
/// lines carry the diff roles. The body **is** the entry's text, so it copies as shown. This
/// result carries no `hunks`, so the body keeps the numberless form (ADR 0028
/// decision 5's fallback): the sign column, no gutter.
#[test]
fn a_patch_card_shows_the_replacement_with_diff_roles() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "patch".into(),
            arguments: serde_json::json!({
                "path": "src/main.rs",
                "old": "let a = 1;\nlet b = 2;",
                "new": "let a = 3;",
            }),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("replaced 1 occurrence in src/main.rs")],
            structured: Some(serde_json::json!({"path": "src/main.rs", "replaced": 1})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    app.tool_display.fold = radian_tui::tools::ToolFold::Expanded;
    let terminal = render(&mut app, 70, 20);
    let text = screen(&terminal);
    assert!(text.contains("◻ patch src/main.rs · +1 −2 ✓"), "{text}");
    let theme = app.theme;
    assert_eq!(
        style_of(&terminal, "-let a = 1;").expect("removed").fg,
        theme.diff_removed.fg
    );
    assert_eq!(
        style_of(&terminal, "+let a = 3;").expect("added").fg,
        theme.diff_added.fg
    );
    assert!(
        style_of(&terminal, "replaced").is_none(),
        "the result text is replaced by the diff: {text}"
    );
}

/// A `write` card: the head counts, the body numbers every content line as an addition
/// (`write` is a whole-file write, ADR 0028), and a CJK path renders.
#[test]
fn a_write_card_numbers_every_content_line() {
    // A CJK path and content: the byte count is UTF-8 bytes, the line count is lines.
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": "笔记.md", "content": "第一行\n第二行"}),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("wrote 19 bytes to 笔记.md")],
            structured: Some(serde_json::json!({"path": "笔记.md", "bytes_written": 19})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    app.tool_display.fold = radian_tui::tools::ToolFold::Expanded;
    let terminal = render(&mut app, 70, 20);
    let text = screen(&terminal);
    // Wide characters make the screen model insert a blank cell after each; compare without
    // spaces so the assertion is about the text, not the terminal's cell model.
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(squeezed.contains("◻write笔记.md·+2✓"), "{text}");
    // The new file has no old content (`previous_lines` absent), so the head stays `+2`, and
    // each content line is a numbered `Added` row. The `+` is the diff-added sign.
    assert!(squeezed.contains("1+第一行"), "{text}");
    assert!(squeezed.contains("2+第二行"), "{text}");
    let sign = style_of(&terminal, "1 +").expect("the line number");
    assert_eq!(
        sign.fg, app.theme.diff_line_number.fg,
        "the line number is the weak diff gutter"
    );
    let body = style_of(&terminal, "第").expect("the content");
    assert_eq!(
        body.fg, app.theme.tool_result.fg,
        "plain content keeps the body style"
    );
}

/// The block layout of a fixed transcript (ADR 0028): one snapshot of every line,
/// plus the background bands. The transcript is a user block, an assistant with a folded
/// thinking channel, a batch of two parallel calls whose results arrive in reverse order, a
/// notice and a compaction summary.
#[test]
fn the_block_layout_snapshot() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: Message::user("explain the failure"),
    });
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: Message::Assistant {
            content: vec![ContentPart::text("Looking into it.")],
            reasoning: vec![ContentPart::text(
                "the build broke after the refactor\nsecond thought\nthird thought",
            )],
            tool_calls: vec![
                ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({"command": "make test"}),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "src"}),
                },
            ],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
    });
    // The results arrive in reverse order; each still hugs its own call (rule 4).
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 2,
        message: Message::Tool {
            result: ToolResult {
                call_id: "c2".into(),
                content: vec![ContentPart::text("file\t512\tb.md\ndir\t4096\tsrc")],
                structured: Some(
                    serde_json::json!({"path": "src", "entries": 2, "total_entries": 2}),
                ),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        },
    });
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 3,
        message: Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(
                    "$ make test\nexit code: 2\n--- stdout ---\nboom",
                )],
                structured: Some(serde_json::json!({"command": "make test", "exit_code": 2})),
                is_error: true,
                error_code: None,
            },
            meta: Default::default(),
        },
    });
    app.push_notice("profile: workspace · sandbox macos-seatbelt");
    let history =
        radian_core::compaction::compacted_history("the earlier work, summarized", &[], &[])
            .expect("a valid history");
    app.push_message(&history[0]);

    // Tall enough that the whole transcript is on screen (the view follows the bottom).
    let terminal = render(&mut app, 80, 40);
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..40u16)
        .map(|y| {
            let line: String = (0..80u16)
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|c| c.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect();
            line.trim_end().to_string()
        })
        .collect();
    // The status glyphs the head ends with (ADR 0028).
    let status = app.footer_glyphs().tool_status();
    let golden: Vec<String> = vec![
        // ADR 0035: the thin user row is `❯ 文本`, one line, no band and
        // no half-cell padding. The container's one blank line separates it from the block below.
        "❯ explain the failure".to_string(), // the user text
        "".to_string(),                      // the container gap after the user row
        "◼ thinking · 3 lines".to_string(), // the folded channel: the unicode fold glyph (ADR 0035's 2026-10-16 note)
        "".to_string(), // ADR 0035: one blank line between thinking and the block below
        "  Looking into it.".to_string(), // the reply
        "".to_string(), // the gap
        // The failed shell block (ADR 0028): one head line, `│ ` body rows.
        "◻ shell $ make test · exit 2 ×".to_string(),
        "│ $ make test".to_string(), // the tool's own text leads with the command
        "│ exit code: 2".to_string(),
        "│ --- stdout ---".to_string(),
        "│ boom".to_string(),
        "".to_string(), // the gap
        // The `read` block on a directory: a compact tool, so its head is the `•` one-liner
        // whatever its result hides (ADR 0035's 2026-10-16 note, option A; the listing text
        // is not laid out under `errors`).
        format!("• read src {}", status[1]),
        "".to_string(), // the gap
        "* profile: workspace · sandbox macos-seatbelt".to_string(),
        "".to_string(), // the gap above the rule line
        "── context compacted ──".to_string(),
        "".to_string(), // the gap below it
    ];
    assert_eq!(rows[..golden.len()], golden[..], "{rows:#?}");
    // No band anywhere in the transcript (ADR 0028 + ADR 0035
    // decision 10): the user row, the assistant rows and the tool rows all sit on the terminal's
    // own background.
    for y in 0..golden.len() as u16 {
        for x in [0u16, 79] {
            assert_eq!(
                buffer.cell((x, y)).map(|c| c.bg),
                Some(Color::Reset),
                "row {y} col {x} carries no background"
            );
        }
    }
}

/// A tab is four columns when the wrap decides where to break (ADR 0028 R9), not zero: a line
/// that fits at six columns plus a tab does **not** fit in nine.
#[test]
fn a_tab_wraps_as_four_columns() {
    // Six columns + a tab = 10 > 9: the tab moves to the next line (the wrap keeps the
    // source char; the layout is what renders it as four spaces).
    let wrapped = radian_tui::render::wrap("abcdef\t", 9);
    assert_eq!(
        wrapped,
        vec!["abcdef".to_string(), "\t".to_string()],
        "{wrapped:?}"
    );
    // At width 10 it fits on one line.
    let wrapped = radian_tui::render::wrap("abcdef\t", 10);
    assert_eq!(wrapped.len(), 1, "{wrapped:?}");
    // The counting path agrees (the corpus test pins the rest).
    assert_eq!(radian_tui::render::wrap_line_count("abcdef\t", 9), 2);
}

/// The ask and memory renderers' heads.
#[test]
fn the_ask_and_memory_cards_have_their_own_heads() {
    for (name, arguments, structured, expected) in [
        (
            "ask_user",
            serde_json::json!({"questions": [{"id": "q1"}, {"id": "q2"}]}),
            serde_json::json!({"request_id": "r", "answers": []}),
            // ADR 0028: the record is an open frame, not the plain head.
            "╭─ ask",
        ),
        (
            "memory_search",
            serde_json::json!({"ids": ["mem-7"]}),
            serde_json::json!({}),
            "◻ memory_search mem-7 ✓",
        ),
    ] {
        let mut app = App::new("m", "s");
        app.push_message(&assistant_with(
            "",
            "",
            false,
            vec![ToolCall {
                id: "c1".into(),
                name: name.into(),
                arguments,
            }],
        ));
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text("body line")],
                structured: Some(structured),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        });
        let text = screen(&render(&mut app, 70, 12));
        assert!(text.contains(expected), "{name}: {text}");
    }
}

/// A compaction summary (ADR 0028 R5): one line, expandable to the summary and the user's
/// verbatim words; the live path adds the revision and token counts, the resume replay shows
/// the same line without them (the journaled message does not carry them).
#[test]
fn a_compaction_summary_is_one_line_that_expands() {
    use radian_core::compaction::{RetainedUserMessage, compacted_history};
    use radian_tui::tools::ToolFold;
    let history = compacted_history(
        "the earlier work, summarized",
        &[RetainedUserMessage {
            text: "the user's own words".to_string(),
        }],
        &[],
    )
    .expect("a valid history");
    let summary = history[0].clone();

    // Resume: the message alone, no revision or counts.
    let mut resumed = App::new("m", "s");
    resumed.push_message(&summary);
    let text = screen(&render(&mut resumed, 70, 12));
    assert!(text.contains("── context compacted ──"), "{text}");
    assert!(
        !text.contains("the earlier work"),
        "folded by default: {text}"
    );

    // Expanded (Ctrl-O) shows the summary and the verbatim user words, and they copy.
    resumed.tool_display.fold = ToolFold::Expanded;
    let text = screen(&render(&mut resumed, 70, 20));
    assert!(text.contains("the earlier work, summarized"), "{text}");
    assert!(text.contains("the user's own words"), "{text}");

    // Live: the same message after a `CompactionFinished` event carries the numbers.
    let mut live = App::new("m", "s");
    live.apply_event(&AgentEvent::CompactionFinished {
        revision: 3,
        tokens_before: 12_000,
        tokens_after: 4_000,
        below_target: false,
        deterministic: false,
        dropped: 0,
    });
    live.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: summary.clone(),
    });
    let text = screen(&render(&mut live, 70, 12));
    assert!(
        text.contains("── context compacted (rev 3, 12000 → 4000) ──"),
        "{text}"
    );
    // The two paths differ only in that line: the entries' data is otherwise the same.
    let resumed_info = resumed
        .session
        .entries
        .iter()
        .find_map(|entry| entry.compaction.as_deref())
        .expect("the summary entry");
    let live_info = live
        .session
        .entries
        .iter()
        .find_map(|entry| entry.compaction.as_deref())
        .expect("the summary entry");
    assert_eq!(resumed_info.summary, live_info.summary);
    assert_eq!(resumed_info.user_messages, live_info.user_messages);
    assert_eq!(resumed_info.revision, None);
    assert_eq!(live_info.revision, Some(3));
    assert_eq!(live_info.tokens_before, Some(12_000));
    assert_eq!(live_info.tokens_after, Some(4_000));
}

/// The folded summary line is one line; expanding changes the transcript's line count (the
/// fold is a layout cache key, override included).
#[test]
fn the_summary_fold_changes_the_line_count() {
    use radian_core::compaction::compacted_history;
    use radian_tui::tools::ToolFold;
    let history = compacted_history("one\ntwo\nthree\nfour", &[], &[]).expect("valid");
    let mut app = App::new("m", "s");
    app.push_message(&history[0]);
    let _ = render(&mut app, 70, 20);
    let folded = app.session.total_lines;
    assert_eq!(folded, 1);
    app.tool_display.fold = ToolFold::Expanded;
    let _ = render(&mut app, 70, 20);
    assert_eq!(app.session.total_lines, 4, "the summary's four lines");
    app.tool_display.fold = ToolFold::Collapsed;
    let _ = render(&mut app, 70, 20);
    assert_eq!(app.session.total_lines, 1, "folded again");
}

/// An MCP tool's alias (`server__tool`) never reaches a built-in renderer: its arguments and
/// output are its server's own, so it takes the generic card (name + argument JSON), while the
/// exact built-in names keep their own renderers. The reviewer's T3.
#[test]
fn an_mcp_alias_renders_generic_and_the_builtin_keeps_its_card() {
    fn card_for(name: &str, arguments: serde_json::Value) -> App {
        let mut app = App::new("m", "s");
        app.push_message(&assistant_with(
            "",
            "",
            false,
            vec![ToolCall {
                id: "c1".into(),
                name: name.into(),
                arguments,
            }],
        ));
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text("file contents")],
                structured: None,
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        });
        app
    }
    // An MCP-provided `read`: the generic head (name + the arguments' JSON), not `read: path`.
    let text = screen(&render(
        &mut card_for("fs__read", serde_json::json!({"file_path": "a.rs"})),
        70,
        12,
    ));
    assert!(text.contains("fs__read"), "{text}");
    assert!(
        text.contains("file_path"),
        "the argument JSON summary: {text}"
    );
    assert!(
        !text.contains("• read a.rs"),
        "not the built-in read card: {text}"
    );
    // Same for a `shell` and a `list` from a server.
    let text = screen(&render(
        &mut card_for("x__shell", serde_json::json!({"cmd": "ls"})),
        70,
        12,
    ));
    assert!(text.contains("x__shell"), "{text}");
    assert!(!text.contains("• shell $ ls"), "{text}");
    let text = screen(&render(
        &mut card_for("mcp__github__list", serde_json::json!({"repo": "r"})),
        70,
        12,
    ));
    assert!(text.contains("mcp__github__list"), "{text}");
    // The exact built-in still gets its own renderer.
    let text = screen(&render(
        &mut card_for(
            "read",
            serde_json::json!({"path": "a.rs", "offset": 3, "limit": 2}),
        ),
        70,
        12,
    ));
    assert!(text.contains("• read a.rs 4-5 ✓"), "{text}");
}

/// A batch of calls pairs each result with **its own** call by id: results arriving in
/// reverse order (one success, one failure) still land on the right cards. Covered on the live
/// commit path and on the resume replay path.
#[test]
fn a_batch_of_calls_pairs_its_results_by_id_in_any_order() {
    fn batch() -> Message {
        assistant_with(
            "",
            "",
            false,
            vec![
                ToolCall {
                    id: "c1".into(),
                    name: "generic_a".into(),
                    arguments: serde_json::json!({"n": 1}),
                },
                ToolCall {
                    id: "c2".into(),
                    name: "generic_b".into(),
                    arguments: serde_json::json!({"n": 2}),
                },
            ],
        )
    }
    fn results() -> [Message; 2] {
        [
            // The SECOND call's result arrives first, and it failed.
            Message::Tool {
                result: ToolResult {
                    call_id: "c2".into(),
                    content: vec![ContentPart::text("b failed\nb detail\nb third")],
                    structured: None,
                    is_error: true,
                    error_code: None,
                },
                meta: Default::default(),
            },
            Message::Tool {
                result: ToolResult {
                    call_id: "c1".into(),
                    content: vec![ContentPart::text("a ok")],
                    structured: None,
                    is_error: false,
                    error_code: None,
                },
                meta: Default::default(),
            },
        ]
    }

    let mut live = App::new("m", "s");
    live.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: batch(),
    });
    for (i, message) in results().into_iter().enumerate() {
        live.apply_event(&AgentEvent::TranscriptAppended {
            index: i + 1,
            message,
        });
    }
    let mut resumed = App::new("m", "s");
    resumed.push_message(&batch());
    for message in results() {
        resumed.push_message(&message);
    }

    for app in [&mut live, &mut resumed] {
        let calls: Vec<&Entry> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ToolCall)
            .collect();
        assert_eq!(calls.len(), 2, "one card per call");
        let a = calls
            .iter()
            .find(|entry| entry.tool.as_ref().is_some_and(|c| c.name == "generic_a"))
            .expect("card a");
        let b = calls
            .iter()
            .find(|entry| entry.tool.as_ref().is_some_and(|c| c.name == "generic_b"))
            .expect("card b");
        assert_eq!(a.result, Some(false), "a is the success");
        assert_eq!(b.result, Some(true), "b is the failure");
        // The result entries carry their own call's data too.
        let result_b = app
            .session
            .entries
            .iter()
            .find(|entry| {
                entry.kind == EntryKind::ToolResult
                    && entry.tool.as_ref().is_some_and(|c| c.name == "generic_b")
            })
            .expect("b's result entry");
        assert!(result_b.is_error);
        let result_a = app
            .session
            .entries
            .iter()
            .find(|entry| {
                entry.kind == EntryKind::ToolResult
                    && entry.tool.as_ref().is_some_and(|c| c.name == "generic_a")
            })
            .expect("a's result entry");
        assert!(!result_a.is_error);
    }
    // And the failure's body is the one expanded: `a ok`'s body is not shown as a failure.
    let text = screen(&render(&mut live, 60, 16));
    assert!(text.contains("b failed"), "{text}");
}

/// ADR 0028 R4's fold interpretation: `expanded` shows everything, `errors` the last
/// `tool_preview_lines` of a success, `collapsed` only the header; a failure is fully expanded
/// in all three. (The reviewer's T2: `collapsed` and `errors` used to be indistinguishable.)
#[test]
fn collapsed_and_errors_differ_for_a_successful_card() {
    use radian_tui::tools::ToolFold;
    fn app_with(is_error: bool) -> App {
        let mut app = App::new("m", "s");
        app.push_message(&assistant_with(
            "",
            "",
            false,
            vec![ToolCall {
                id: "c1".into(),
                name: "generic_tool".into(),
                arguments: serde_json::json!({}),
            }],
        ));
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(
                    (1..=20).map(|i| format!("line {i}\n")).collect::<String>(),
                )],
                structured: None,
                is_error,
                error_code: None,
            },
            meta: Default::default(),
        });
        app
    }
    let mut counts = Vec::new();
    for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
        let mut app = app_with(false);
        app.tool_display.fold = fold;
        let _ = render(&mut app, 60, 30);
        counts.push(app.session.total_lines);
    }
    // The empty assistant entry is skipped entirely (no lines and no blank line of its own,
    // rule 1), so the tool block is the transcript's first one: its head row plus its body
    // rows - 20 rows expanded, the last 5 under `errors`, none under `collapsed` (the head
    // only).
    assert_eq!(counts, vec![21, 6, 1], "expanded / errors / collapsed");
    // A failure: the same count in all three, and the body really visible.
    for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
        let mut app = app_with(true);
        app.tool_display.fold = fold;
        let text = screen(&render(&mut app, 60, 40));
        assert!(text.contains("line 1 "), "{fold:?}: {text}");
        assert!(text.contains("line 20"), "{fold:?}");
    }
}

/// A failed call's body is never hidden (ADR 0028 R4), whatever the fold mode.
#[test]
fn an_error_card_stays_expanded_under_collapsed() {
    use radian_tui::tools::ToolFold;
    let mut app = App::new("m", "s");
    app.tool_display.fold = ToolFold::Collapsed;
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "generic_tool".into(),
                arguments: serde_json::json!({}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
    });
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                // Longer than the preview, so a wrong fold would really hide it.
                content: vec![ContentPart::text(
                    (1..=20).map(|i| format!("boom {i}\n")).collect::<String>(),
                )],
                structured: None,
                is_error: true,
                error_code: None,
            },
            meta: Default::default(),
        },
    });
    // Tall enough that the whole transcript is on screen (the view follows the bottom).
    let text = screen(&render(&mut app, 60, 40));
    // The distinctive lines at both ends: a fold would show only the last preview lines.
    assert!(
        text.contains("boom 1\n") || text.contains("boom 1 "),
        "the first line: {text}"
    );
    assert!(text.contains("boom 20"), "the last line: {text}");
    assert!(!text.contains("more lines"), "nothing folded: {text}");
}

/// The `shell` renderer's body marks the stderr section with the warning role (the tool's
/// text separates the streams; the structured value has byte counts only).
#[test]
fn a_shell_body_marks_stderr_with_the_warning_role() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command": "run"}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
    });
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: Message::Tool {
            result: ToolResult {
                call_id: "c1".into(),
                content: vec![ContentPart::text(
                    "$ run\nexit code: 1\n--- stdout ---\nhello\n--- stderr ---\nBOOM",
                )],
                structured: Some(serde_json::json!({"command": "run", "exit_code": 1})),
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        },
    });
    // Ctrl-O to expanded, so the whole body shows.
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let terminal = render(&mut app, 60, 14);
    let text = screen(&terminal);
    assert!(text.contains("BOOM"), "{text}");
    let warn = style_of(&terminal, "BOOM").expect("the stderr line");
    assert_eq!(
        warn.fg, app.theme.note.fg,
        "stderr renders on the warning level"
    );
}

/// Thinking folds (ADR 0028 R3): collapsed is one placeholder line, Ctrl-T expands it to the
/// full text, and toggling back collapses it — the fold state is part of the layout cache
/// key, so the count changes with it.
#[test]
fn thinking_folds_to_one_line_and_ctrl_t_expands_it() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: assistant_with("THE ANSWER", "FIRST THOUGHT\nSECOND THOUGHT", false, vec![]),
    });
    let text = screen(&render(&mut app, 60, 10));
    assert!(text.contains("◼ thinking · 2 lines"), "{text}");
    assert!(
        !text.contains("FIRST THOUGHT"),
        "folded by default:
{text}"
    );
    assert!(text.contains("THE ANSWER"), "{text}");

    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let text = screen(&render(&mut app, 60, 12));
    assert!(
        text.contains("FIRST THOUGHT"),
        "expanded:
{text}"
    );
    assert!(text.contains("SECOND THOUGHT"), "{text}");
    assert!(!text.contains("◼ thinking"), "{text}");

    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let text = screen(&render(&mut app, 60, 10));
    assert!(text.contains("◼ thinking · 2 lines"), "{text}");
    assert!(!text.contains("FIRST THOUGHT"), "{text}");
}

/// The fold state is part of the **layout cache key** (ADR 0028 R1): collapsing changes the
/// transcript's line count, so the viewport math (total lines, scroll range) changes with it.
/// A cache that forgot the mode would keep serving the old count.
#[test]
fn folding_changes_the_transcript_line_count() {
    let mut app = App::new("m", "s");
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: assistant_with("ANSWER", "one\ntwo\nthree", false, vec![]),
    });
    let _ = render(&mut app, 60, 10);
    let expanded = app.session.total_lines;
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Collapsed;
    let _ = render(&mut app, 60, 10);
    let collapsed = app.session.total_lines;
    assert_eq!(
        expanded,
        collapsed + 3,
        "three thinking lines plus the header become one placeholder"
    );
    // The placeholder, one blank line (ADR 0035) and the answer.
    assert_eq!(collapsed, 3, "placeholder + the one-line gap + answer");
}

/// `tui.display.reasoning = "hidden"` removes the reasoning lines entirely.
#[test]
fn hidden_reasoning_contributes_nothing() {
    let mut app = App::new("m", "s");
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Hidden;
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 0,
        message: assistant_with("THE ANSWER", "SECRET THOUGHT", false, vec![]),
    });
    let text = screen(&render(&mut app, 60, 10));
    assert!(text.contains("THE ANSWER"), "{text}");
    assert!(!text.contains("SECRET THOUGHT"), "{text}");
    assert!(!text.contains("thinking"), "{text}");
}

/// `split_label`: a leading `[tag]`, or the short prefix before the first colon. Nothing is
/// special-cased per notice — the startup lines and the runtime notices both go through it.
#[test]
fn the_structured_label_split_is_one_rule() {
    use radian_tui::render::split_label;
    assert_eq!(
        split_label("profile: workspace · sandbox macos"),
        Some(("profile:", " workspace · sandbox macos"))
    );
    assert_eq!(
        split_label("[memory] no project linked"),
        Some(("[memory]", " no project linked"))
    );
    assert_eq!(split_label("context: C ?"), Some(("context:", " C ?")));
    // No label: a colon far in, an empty prefix, a `[` that never closes.
    assert_eq!(split_label("just a sentence"), None);
    assert_eq!(
        split_label("a very long prefix that keeps going way past the cap: value"),
        None
    );
    assert_eq!(split_label(": empty prefix"), None);
    assert_eq!(split_label("[unclosed tag"), None);
}

/// A startup notice's label carries the accent role; its body stays on the secondary level,
/// and the dark builtin keeps ui-r1's readable default foreground for the body.
#[test]
fn a_startup_line_colors_its_label_with_accent() {
    let mut app = App::new("m", "s");
    app.push_notice("profile: workspace · sandbox macos-seatbelt");
    let terminal = render(&mut app, 60, 6);
    let label = style_of(&terminal, "profile:").expect("the label is on screen");
    assert_eq!(
        label.fg,
        Some(Color::Rgb(167, 152, 215)),
        "accent = pi's violet"
    );
    assert!(
        label.add_modifier.contains(ratatui::style::Modifier::BOLD),
        "the label is the emphasis"
    );
    let body = style_of(&terminal, "workspace").expect("the body is on screen");
    assert_eq!(
        body.fg,
        Some(Color::Rgb(157, 165, 169)),
        "the body is pi's muted level"
    );
}

/// ADR 0035: the user row has no band and no half-cell padding at any
/// width — the `user` role is a foreground role. Pinned at 80 and 120 columns (the widths the
/// hand-test list uses).
#[test]
fn the_user_row_has_no_band_at_80_and_120() {
    for width in [80u16, 120] {
        let mut app = App::new("m", "s");
        classic_footer(&mut app);
        app.push_message(&Message::user("hello band"));
        let terminal = render(&mut app, width, 8);
        let text = screen(&terminal);
        assert!(
            !text.contains('▄') && !text.contains('▀'),
            "{width}: {text}"
        );
        assert!(row(&terminal, 0).starts_with("❯ hello band"), "{width}");
        // Every cell of the row keeps the terminal background: no band, trailing cells included.
        for col in [0u16, 2, width - 1] {
            let style = cell_style(&terminal, col, 0);
            assert_eq!(style.bg, Some(Color::Reset), "{width} col {col}: {style:?}");
        }
    }
}

/// A tool block has **no** band, whatever its state (ADR 0028): every cell of the head row and of the body rows sits
/// on the terminal's own background.
#[test]
fn a_tool_block_never_paints_a_band() {
    for (result, is_error) in [(None, false), (Some(false), false), (Some(true), true)] {
        let mut app = App::new("m", "s");
        classic_footer(&mut app);
        let mut call = Entry::new(EntryKind::ToolCall, "shell $ ls");
        call.result = result;
        let mut card =
            radian_tui::tools::ToolCard::new("c1", "shell", serde_json::json!({"command": "ls"}));
        card.structured = Some(serde_json::json!({"command": "ls", "exit_code": 0}));
        call.tool = Some(Box::new(card));
        app.session.entries.push(call);
        if let Some(result) = result {
            let mut entry = Entry::new(EntryKind::ToolResult, "line one\nline two");
            entry.is_error = is_error;
            entry.result = Some(result);
            entry.tool = Some(Box::new(radian_tui::tools::ToolCard::new(
                "c1",
                "shell",
                serde_json::json!({"command": "ls"}),
            )));
            app.session.entries.push(entry);
        }
        let terminal = render(&mut app, 40, 10);
        let buffer = terminal.backend().buffer();
        for row in 0..3u16 {
            for col in 0..40u16 {
                assert_eq!(
                    buffer.cell((col, row)).expect("on screen").bg,
                    Color::Reset,
                    "row {row} col {col} carries no background"
                );
            }
        }
    }
}

/// The `/login` wizard renders with the theme's text levels (ADR 0028): the prompt is main
/// text, the selected choice the emphasis role, the others secondary, and the key hints weak.
#[test]
fn the_login_wizard_uses_the_text_hierarchy() {
    use radian_tui::login::{LoginStep, LoginWizard};
    let mut app = App::new("m", "s");
    app.push_modal(radian_tui::app::Modal::Login(Box::new(LoginWizard::new(
        LoginStep::ChooseProvider { cursor: 0 },
    ))));
    let terminal = render(&mut app, 70, 14);
    let text = screen(&terminal);
    assert!(text.contains("choose a provider"), "{text}");
    let prompt = style_of(&terminal, "choose").expect("the prompt");
    assert_eq!(prompt.fg, app.theme.text.fg, "the prompt is main text");
    // ADR 0035: the key hints live in the panel's bottom border (dim keys).
    let hint = style_of(&terminal, "↑/↓").expect("the key hint");
    assert_eq!(hint.fg, app.theme.hint.fg, "the key hint is weak");
    // The first choice row is the selection: the shared list's emphasis (bold cursor).
    let selected = style_of(&terminal, "❯").expect("the selected row");
    assert!(
        selected
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "the selected row is emphasized"
    );
}

/// The startup trust modal (ADR 0031 T3, ADR 0035): the shared panel with the
/// choices through the shared list, at 80/120 columns and in the ascii tier. The sandbox
/// choice comes first and the cursor's default is on it.
#[test]
fn the_trust_panel_snapshot_at_80_120_and_ascii() {
    let mut app = App::new("m", "s");
    app.push_trust_modal(std::path::PathBuf::from("/work/example"), None, None);

    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("╭─ project trust "), "{text}");
    assert!(
        text.contains("project resources found in /work/example"),
        "{text}"
    );
    assert!(
        text.contains("❯ Trust this directory, and use the sandbox for this project"),
        "the sandbox choice is first and pre-selected: {text}"
    );
    assert!(
        text.contains("↑/↓ choose · ⏎ select · esc do not trust"),
        "{text}"
    );

    let terminal = render(&mut app, 120, 24);
    let text = screen(&terminal);
    assert!(text.contains("╭─ project trust "), "{text}");
    assert!(
        text.contains("Trust the parent directory (/work)"),
        "{text}"
    );

    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".into()),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("+- project trust "), "{text}");
    assert!(
        text.contains("> Trust this directory, and use the sandbox"),
        "{text}"
    );
    assert!(!text.contains('╭'), "{text}");
}

/// Slice B2: the trust modal lists the project `.mcp.json` servers — names, commands as
/// written, env key names only (never values) — and marks remote entries skipped. The
/// mutation (the modal drops the preview rows) fails this test.
#[test]
fn the_trust_panel_lists_the_project_mcp_preview() {
    let mut app = App::new("m", "s");
    app.push_trust_modal(
        std::path::PathBuf::from("/work/example"),
        Some(radian_host::ProjectPreview {
            entries: vec![
                radian_host::ProjectPreviewEntry {
                    name: "fs".to_owned(),
                    command: Some("npx ${MCP_B2_BIN}".to_owned()),
                    args: vec!["-y".to_owned()],
                    env_keys: vec!["FIXTURE_LOG".to_owned()],
                    skipped: None,
                },
                radian_host::ProjectPreviewEntry {
                    name: "web".to_owned(),
                    command: None,
                    args: Vec::new(),
                    env_keys: Vec::new(),
                    skipped: Some("type `http`".to_owned()),
                },
            ],
            error: None,
        }),
        None,
    );
    let terminal = render(&mut app, 100, 30);
    let text = screen(&terminal);
    assert!(
        text.contains("project .mcp.json: 1 server to load once trusted"),
        "{text}"
    );
    assert!(
        text.contains("mcp: fs `npx ${MCP_B2_BIN} -y` env: FIXTURE_LOG"),
        "commands as written, key names only: {text}"
    );
    assert!(text.contains("mcp: web skipped (type `http`)"), "{text}");
    assert!(!text.contains("sekret"), "values never leak: {text}");
}

/// Slice B (hooks): the trust modal lists the project `.radian/hooks.json` hooks — the
/// event, the matcher and the command as written. The mutation (the modal drops the
/// hooks preview rows) fails this test.
#[test]
fn the_trust_panel_lists_the_project_hooks_preview() {
    let mut app = App::new("m", "s");
    app.push_trust_modal(
        std::path::PathBuf::from("/work/example"),
        None,
        Some(radian_host::ProjectHooksPreview {
            entries: vec![
                radian_host::ProjectHooksPreviewEntry {
                    event: "PreToolUse".to_owned(),
                    matcher: "patch".to_owned(),
                    command: "make check".to_owned(),
                },
                radian_host::ProjectHooksPreviewEntry {
                    event: "Stop".to_owned(),
                    matcher: "*".to_owned(),
                    command: "make verify".to_owned(),
                },
            ],
            error: None,
        }),
    );
    let terminal = render(&mut app, 100, 30);
    let text = screen(&terminal);
    assert!(
        text.contains("project .radian/hooks.json: 2 hooks to load once trusted"),
        "{text}"
    );
    assert!(
        text.contains("hooks: PreToolUse patch `make check`"),
        "event, matcher, command: {text}"
    );
    assert!(text.contains("hooks: Stop * `make verify`"), "{text}");
}

/// The hooks page's Recent runs section shows the run records newest first, in the one
/// record format (`<event>:<tool> [<command>] <end> (<n>ms)`; `Stop` has no tool name).
/// Dropping the section, or reversing the order, fails this.
#[test]
fn the_hooks_page_lists_recent_runs_newest_first() {
    use radian_host::{HookRunEnd, HookRunRecord};
    use radian_tui::app::Overlay;
    use radian_tui::command::InspectorPage;
    use radian_tui::inspector::{HookEntry, Inspector, InspectorSnapshot};
    let snapshot = InspectorSnapshot {
        hooks: vec![HookEntry {
            event: "PreToolUse".to_owned(),
            matcher: "*".to_owned(),
            command: "make lint".to_owned(),
            timeout_secs: 10,
            source: "user config.toml".to_owned(),
            level: "user".to_owned(),
            effective: true,
        }],
        hook_runs: vec![
            HookRunRecord {
                event: "PreToolUse".to_owned(),
                tool: Some("shell".to_owned()),
                command: "make lint".to_owned(),
                end: HookRunEnd::Status(0),
                millis: 12,
            },
            HookRunRecord {
                event: "Stop".to_owned(),
                tool: None,
                command: "make check".to_owned(),
                end: HookRunEnd::TimedOut,
                millis: 60_000,
            },
        ],
        ..Default::default()
    };
    let mut app = App::new("m", "s");
    app.push_overlay(Overlay::Inspector(Inspector::new(
        InspectorPage::Hooks,
        snapshot,
    )));
    let terminal = render(&mut app, 100, 30);
    let text = screen(&terminal);
    assert!(text.contains("Recent runs"), "{text}");
    assert!(
        text.contains("PreToolUse:shell [make lint] status 0 (12ms)"),
        "the tool event names the tool: {text}"
    );
    assert!(
        text.contains("Stop [make check] timed out (60000ms)"),
        "Stop has no tool name: {text}"
    );
    assert!(
        text.find("PreToolUse:shell").unwrap() < text.find("Stop [make check]").unwrap(),
        "newest first: {text}"
    );
}

/// The session picker (ADR 0035/2): the shared panel with the filter row, the
/// sessions through the shared list, and the selected `--resume <id>` on the top border.
#[test]
fn the_picker_panel_snapshot_at_80_120_and_ascii() {
    use radian_host::sessions::SessionRow;
    use radian_tui::app::Overlay;
    use radian_tui::picker::Picker;
    let rows = vec![
        SessionRow {
            session_id: "alpha".into(),
            created_ts: None,
            last_ts: Some("2026-10-15T00:00:00Z".into()),
            message_count: 2,
            summary: "the first session".into(),
        },
        SessionRow {
            session_id: "beta".into(),
            created_ts: None,
            last_ts: Some("2026-10-15T01:00:00Z".into()),
            message_count: 5,
            summary: "the second session".into(),
        },
    ];
    let mut app = App::new("m", "s");
    app.push_overlay(Overlay::Picker(Picker::new(rows, None, "/w")));

    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("╭─ sessions "), "{text}");
    assert!(text.contains("filter: "), "{text}");
    assert!(text.contains("❯ alpha"), "{text}");
    assert!(text.contains("--resume alpha"), "the copyable hint: {text}");
    assert!(
        text.contains("↑/↓ select · ⏎ resume · n new · tab search · esc close"),
        "{text}"
    );

    let terminal = render(&mut app, 120, 24);
    let text = screen(&terminal);
    assert!(text.contains("╭─ sessions "), "{text}");
    assert!(text.contains("the second session"), "{text}");

    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".into()),
        ..Default::default()
    });
    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("+- sessions "), "{text}");
    assert!(text.contains("> alpha"), "{text}");
    assert!(!text.contains('╭'), "{text}");
}

/// The display modes most tests lay out with: both folds expanded, so a test's geometry does
/// not depend on the fold defaults.
#[allow(dead_code)]
const EXPANDED: DisplayModes = DisplayModes {
    reasoning: ReasoningMode::Expanded,
    tools: radian_tui::tools::ToolDisplay {
        fold: radian_tui::tools::ToolFold::Expanded,
        preview_lines: 5,
    },
    mermaid: true,
    highlight: true,
    glyphs: radian_tui::footer::Glyphs::Unicode,
};

/// The question form's key line stays visible when the option list fills the box (same bug the
/// approval box had: the hint used to be the content's last line, and the clip cut it).
#[test]
fn the_question_keys_stay_visible_with_a_long_option_list() {
    let options: Vec<_> = (0..20)
        .map(|index| option(&format!("o{index}"), &format!("Option {index}")))
        .collect();
    let mut app = App::new("m", "s");
    app.update(radian_tui::app::Input::Pending(
        Some(Box::new(PendingView {
            request_id: 7,
            origin: "session".into(),
            request: PendingRequest::Question(radian_coding::QuestionRequest {
                questions: vec![radian_coding::Question {
                    header: None,
                    id: "q".into(),
                    prompt: "Pick one".into(),
                    kind: QuestionKind::Single,
                    options,
                    allow_other: false,
                    required: true,
                }],
            }),
        })),
        1,
    ));
    let text = screen(&render(&mut app, 60, 18));
    assert!(
        text.contains("╰─ space select · ⏎ next · ↑/↓ move"),
        "the keys are in the bottom border:\n{text}"
    );
    assert!(text.contains("Ask · session"), "{text}");
}

/// ADR 0024 rule 4: the Ask panel title is `Ask` alone when the asker is this
/// process's own session, and `Ask · <origin>` only when another session is asking. A TUI
/// process has one session today, so `origin == own session` is the
/// clean test.
#[test]
fn the_ask_panel_title_omits_the_own_session() {
    let questions = vec![radian_coding::Question {
        id: "q".into(),
        header: None,
        prompt: "Pick one".into(),
        kind: QuestionKind::Text,
        options: vec![],
        allow_other: false,
        required: true,
    }];
    let theme = radian_tui::theme::Theme::dark();
    let title = |origin: &str, own: &str| {
        let form = radian_tui::question::QuestionForm::new(7, origin.into(), questions.clone());
        text_of(&render::question_panel(
            &form,
            own,
            60,
            usize::MAX,
            &theme,
            radian_tui::footer::Glyphs::Unicode,
        ))
    };
    let own = title("session-abc", "session-abc");
    assert!(
        own.contains("╭─ Ask "),
        "own session shows a bare Ask: {own}"
    );
    assert!(
        !own.contains("session-abc"),
        "the own session id never reaches the title: {own}"
    );
    let other = title("other-session", "session-abc");
    assert!(
        other.contains("Ask · other-session"),
        "another session keeps its origin: {other}"
    );
}

/// ADR 0024 rules 1-2: while the ask is pending the transcript shows one weak line
/// ("the questions are in the panel below"), now the open frame's top line; once the answer
/// arrives the **same** entry draws the full open block, and the result entry stays blank, so
/// the ask is never drawn twice.
#[test]
fn a_pending_ask_is_one_line_and_becomes_the_block_once_answered() {
    let mut app = App::new("m", "s");
    let arguments = serde_json::json!({"questions":[
        {"id":"pick","kind":"single","prompt":"Pick one","options":[
            {"id":"a","label":"Option A"},{"id":"b","label":"Option B"}]},
        {"id":"note","kind":"text","prompt":"Anything else?"}
    ]});
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "ask_user".into(),
            arguments,
        }],
    ));

    // Pending: one line - the open frame's top - no questions or options, and only one ask line.
    let pending = screen(&render(&mut app, 70, 16));
    assert!(
        pending.contains("╭─ ask · 2 questions · answering below"),
        "the pending one-liner is the open frame's top: {pending}"
    );
    assert!(
        !pending.contains("Pick one") && !pending.contains("Option A"),
        "the pending line carries no questions or options: {pending}"
    );
    assert_eq!(
        pending.matches("ask · 2 questions").count(),
        1,
        "only one ask line while pending: {pending}"
    );

    // Answered: the same call entry becomes the full open block, and the result entry draws
    // nothing, so there is exactly one block.
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("Pick one → Option A")],
            structured: Some(
                serde_json::json!({"request_id":"r","answers":[{"id":"pick","selected":["a"]}]}),
            ),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let answered = screen(&render(&mut app, 70, 20));
    assert!(
        answered.contains("╭─ ask"),
        "the same entry draws the open block: {answered}"
    );
    assert!(answered.contains("Pick one"), "{answered}");
    assert!(answered.contains("◉ Option A"), "{answered}");
    assert!(
        !answered.contains("answering below"),
        "the pending line is gone: {answered}"
    );
    assert_eq!(
        answered.matches("╭─ ask").count(),
        1,
        "exactly one block, not two: {answered}"
    );
}

/// ADR 0024 rule 2: a declined ask shows its status line inside the same open frame.
#[test]
fn a_declined_ask_shows_the_status_inside_the_block() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "ask_user".into(),
            arguments: serde_json::json!({"questions":[{"id":"q","kind":"text","prompt":"Say something"}]}),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("user declined to answer")],
            structured: None,
            is_error: true,
            error_code: Some("user_declined".into()),
        },
        meta: Default::default(),
    });
    let declined = screen(&render(&mut app, 70, 16));
    assert!(
        declined.contains("╭─ ask"),
        "the declined record is the open frame: {declined}"
    );
    assert!(
        declined.contains("│ user declined to answer"),
        "the status line sits in the frame's body column: {declined}"
    );
    assert!(
        !declined.contains("answering below"),
        "the pending line is gone: {declined}"
    );
    assert_eq!(
        declined.matches("╭─ ask").count(),
        1,
        "exactly one block: {declined}"
    );
}

/// `wrap_line_count` (the counting-only path, used to measure every entry on the first frame
/// and after a resize) must agree with `wrap` for the same corpus, or the transcript's total
/// line count and the virtualized window disagree.
#[test]
fn the_line_count_matches_the_wrap() {
    let corpus = [
        ("hello world", 5usize),
        ("hello world", 11),
        ("", 5),
        ("\n", 5),
        ("a\n\nb", 5),
        ("  leading and trailing  ", 7),
        ("日本語のテキストは幅で折る", 5),
        ("ab   cd", 4),
        ("one two three four five six seven", 7),
        ("mixed 日本語 text wrap", 6),
        ("日本", 1),
        ("trailing spaces   ", 5),
        ("word\n", 5),
        ("combining a\u{301}b", 2),
        ("a b c d e f", 1),
        ("a\tb", 4),
        ("x\ty\tz", 6),
    ];
    for (text, width) in corpus {
        assert_eq!(
            radian_tui::render::wrap_line_count(text, width),
            radian_tui::render::wrap(text, width).len(),
            "{text:?} at {width}"
        );
    }
}

// ---------- the ask markers (ADR 0024 implementation note) ----------

/// A question of `kind` with `options` and `allow_other`.
fn ask_question(
    kind: QuestionKind,
    options: Vec<radian_coding::QuestionOption>,
    allow_other: bool,
) -> radian_coding::Question {
    radian_coding::Question {
        id: "q".into(),
        header: None,
        prompt: "Pick".into(),
        kind,
        options,
        allow_other,
        required: true,
    }
}

/// A panel over one question, laid out at `width`, as text (the same call the frame makes).
fn ask_panel_text(form: &radian_tui::question::QuestionForm, width: usize) -> String {
    text_of(&render::question_panel(
        form,
        "session",
        width,
        usize::MAX,
        &radian_tui::theme::Theme::dark(),
        radian_tui::footer::Glyphs::Unicode,
    ))
}

/// The chosen option's marker is drawn in its own span and is never bold: the font
/// draws a glyph missing from its bold face at a smaller size (ADR 0024 note; the
/// circle pair of ADR 0028 is used for the same reason).
#[test]
fn the_ask_panel_marker_is_never_bold() {
    let mut form = radian_tui::question::QuestionForm::new(
        7,
        "session".into(),
        vec![ask_question(
            QuestionKind::Single,
            vec![option("a", "Chosen"), option("b", "Other option")],
            true,
        )],
    );
    form.answers[0].selected = vec!["a".into()];
    let lines = render::question_panel(
        &form,
        "session",
        60,
        usize::MAX,
        &radian_tui::theme::Theme::dark(),
        radian_tui::footer::Glyphs::Unicode,
    );
    let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
    terminal
        .draw(|frame| {
            frame.render_widget(ratatui::widgets::Paragraph::new(lines), frame.area());
        })
        .unwrap();

    let marker = style_of(&terminal, "◉").expect("the chosen marker cell");
    assert!(
        !marker.add_modifier.contains(Modifier::BOLD),
        "the chosen marker must not be bold: {marker:?}"
    );
    let label = style_of(&terminal, "Chosen").expect("the label cell");
    assert!(
        label.add_modifier.contains(Modifier::BOLD),
        "the label keeps its chosen (accent) role: {label:?}"
    );
    // The cursor is not bold either.
    let cursor = style_of(&terminal, "❯").expect("the cursor cell");
    assert!(
        !cursor.add_modifier.contains(Modifier::BOLD),
        "the cursor must not be bold: {cursor:?}"
    );
}

/// The transcript record's chosen marker is drawn in its own span and is never bold too: the
/// circle pair is only safe while the marker is not bold (ADR 0028).
#[test]
fn the_transcript_ask_marker_is_never_bold() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "ask_user".into(),
            arguments: serde_json::json!({"questions":[{"id":"q","kind":"single","prompt":"Pick",
                "options":[{"id":"a","label":"Alpha"},{"id":"b","label":"Beta"}]}]}),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text("Pick → Alpha")],
            structured: Some(serde_json::json!({"answers":[{"id":"q","selected":["a"]}]})),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    let terminal = render(&mut app, 70, 18);
    let marker = style_of(&terminal, "◉").expect("the chosen marker in the transcript");
    assert!(
        !marker.add_modifier.contains(Modifier::BOLD),
        "the transcript's chosen marker must not be bold: {marker:?}"
    );
}

/// Single, multi and `Other` all draw the same circle pair (ADR 0028):
/// the checkbox pair is gone.
#[test]
fn the_ask_panel_uses_one_circle_pair_for_single_multi_and_other() {
    let mut single = radian_tui::question::QuestionForm::new(
        7,
        "session".into(),
        vec![ask_question(
            QuestionKind::Single,
            vec![option("a", "Alpha"), option("b", "Beta")],
            true,
        )],
    );
    single.answers[0].selected = vec!["a".into()];
    let text = ask_panel_text(&single, 60);
    assert!(text.contains("◉ Alpha"), "the chosen single option: {text}");
    assert!(
        text.contains("○ Beta"),
        "the unchosen single option: {text}"
    );
    assert!(text.contains("○ Other (type your own)"), "{text}");
    assert!(
        !text.contains('☐')
            && !text.contains('☑')
            && !text.contains("[ ]")
            && !text.contains("[x]"),
        "no checkbox markers: {text}"
    );

    let mut multi = radian_tui::question::QuestionForm::new(
        7,
        "session".into(),
        vec![ask_question(
            QuestionKind::Multi,
            vec![option("a", "Alpha"), option("b", "Beta")],
            false,
        )],
    );
    multi.answers[0].selected = vec!["a".into()];
    let text = ask_panel_text(&multi, 60);
    assert!(text.contains("◉ Alpha"), "{text}");
    assert!(text.contains("○ Beta"), "{text}");
    assert!(!text.contains('☐') && !text.contains('☑'), "{text}");

    let mut other = radian_tui::question::QuestionForm::new(
        7,
        "session".into(),
        vec![ask_question(
            QuestionKind::Single,
            vec![option("a", "Alpha")],
            true,
        )],
    );
    other.answers[0].other = true;
    other.answers[0].cursor = 1;
    let text = ask_panel_text(&other, 60);
    assert!(
        text.contains("◉ Other"),
        "the chosen Other row uses the filled circle, not a checked box: {text}"
    );
}

/// The panel and the transcript block draw the markers from the same source, so both show the
/// circle pair (ADR 0028).
#[test]
fn the_ask_panel_and_the_transcript_block_use_the_same_markers() {
    let mut form = radian_tui::question::QuestionForm::new(
        7,
        "session".into(),
        vec![ask_question(
            QuestionKind::Single,
            vec![option("a", "Alpha"), option("b", "Beta")],
            false,
        )],
    );
    form.answers[0].selected = vec!["a".into()];
    let panel = ask_panel_text(&form, 60);
    assert!(panel.contains('◉') && panel.contains('○'), "{panel}");

    let mut card = radian_tui::tools::ToolCard::new(
        "c1",
        "ask_user",
        serde_json::json!({"questions":[{"id":"q","kind":"single","options":[
            {"id":"a","label":"Alpha"},{"id":"b","label":"Beta"}]}]}),
    );
    card.structured = Some(serde_json::json!({"answers":[{"id":"q","selected":["a"]}]}));
    let mut entry = radian_tui::app::Entry::new(radian_tui::app::EntryKind::ToolResult, "result");
    entry.tool = Some(Box::new(card));
    let block = radian_tui::blocks::layout(&entry, &[], EXPANDED, 60, 24);
    let block_text: String = block
        .iter()
        .map(|line| line.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        block_text.contains('◉') && block_text.contains('○'),
        "the transcript block uses the same pair: {block_text}"
    );
    assert!(
        !block_text.contains('☐') && !block_text.contains('☑'),
        "{block_text}"
    );
}

/// ADR 0003: a reply cut off during reasoning only (empty text, `interrupted`) still has to
/// tell the user that the turn was interrupted. The `[interrupted]` marker is drawn on the
/// entry's **last** body line (`render.rs`'s `entry_lines` / `visible_lines`), so the entry
/// keeps one line for it instead of being skipped as an empty reply.
#[test]
fn an_interrupted_reply_with_no_text_still_shows_the_marker() {
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.push_message(&Message::user("q"));
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("", "a thought", true, vec![]),
    });
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    assert!(
        text.contains("[interrupted]"),
        "the cut-off reply keeps its marker:\n{text}"
    );
    let thought = row_of(&terminal, "a thought").expect("the thinking body");
    let marker = row_of(&terminal, "[interrupted]").expect("the marker row");
    assert_eq!(
        marker,
        thought + 2,
        "one blank row between the thinking block and the marker:\n{text}"
    );
}
