//! Rendering tests on `TestBackend`: reasoning and assistant text are distinguishable,
//! an interrupted prefix is marked, and tool calls are shown.

use agent_core::{AgentEvent, ContentPart, Message, ToolCall, ToolResult};
use radian_coding::QuestionKind;
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

/// ADR 0027 + ADR 0035: the default `bar` status row shows the spinner
/// and the running state while a turn runs (`◜ Working`), and is blank when idle. The dot sits
/// in the rail's column with **one** cell before the text (which starts at the text column);
/// the `lines`/`box` rule line keeps revision 2's two spaces (the box test below).
#[test]
fn the_input_rule_shows_the_spinner_status_while_running() {
    let mut app = App::new("m", "s");
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    // The frame is a function of the injected clock (ADR 0027).
    app.set_now(app.session.spinner_origin);
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(
        text.contains("◜ Working"),
        "the spinner on the bar's status row: {text}"
    );

    // Idle: no spinner, no text.
    app.session.projection.set_turn(radian_tui::TurnState::Idle);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(!text.contains("Working"), "an idle rule is plain: {text}");
    assert!(!text.contains('◜'), "{text}");
}

/// ADR 0018 + ADR 0027: a running compaction shows `Compacting` on the status row
/// (the `CompactionStarted`/`CompactionFinished` events exist, so this is not invented).
#[test]
fn the_input_rule_shows_compacting() {
    let mut app = App::new("m", "s");
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

/// ADR 0027: the `box` style puts the same status on the border's top edge.
#[test]
fn the_box_style_shows_the_spinner_on_the_border() {
    let mut app = App::new("m", "s");
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    app.set_now(app.session.spinner_origin);
    app.input_style = radian_tui::render::InputStyle::Box;
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(
        text.contains("◜  Working"),
        "the box border carries the status: {text}"
    );
    // The rounded corner is still there (the title starts with a space).
    assert!(text.contains('╭'), "{text}");
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
    assert!(shown.contains("❯ ☐ Option 11"), "{shown}");
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
/// This is `tui.input_style = "lines"` (the default is `bar`, ADR 0035 V3).
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

/// The empty input shows the placeholder after the prompt (`lines`), and after the rail in the
/// default `bar` style.
#[test]
fn the_empty_input_shows_the_placeholder_after_the_prompt() {
    let mut app = App::new("m", "s");
    app.input_style = radian_tui::render::InputStyle::Lines;
    let text = screen(&render(&mut app, 80, 8));
    assert!(
        text.contains("❯ (type a message, Enter to send, Ctrl-J for a new line)"),
        "{text}"
    );

    let mut bar = App::new("m", "s");
    let text = screen(&render(&mut bar, 80, 8));
    assert!(
        text.contains("▌ (type a message, Enter to send, Ctrl-J for a new line)"),
        "the bar's placeholder follows the rail:\n{text}"
    );
}

/// `tui.input_style = "box"` is the owner's shape (2026-10-14): a **rounded** border and the
/// `❯` prompt (which the `lines` style uses too, ).
#[test]
fn the_box_input_style_draws_the_rounded_box_with_the_prompt() {
    let mut app = App::new("m", "s");
    app.input = "first\nsecond".into();
    app.input_style = radian_tui::render::InputStyle::Box;
    let text = screen(&render(&mut app, 20, 8));
    assert!(text.contains(" message "), "the box title is back:\n{text}");
    assert!(
        text.contains("│❯ first"),
        "the text starts after the border and the prompt:\n{text}"
    );
    assert!(
        !text.contains("> first"),
        "the old square prompt is gone:\n{text}"
    );
    assert!(
        text.contains('╭') && text.contains('╮') && text.contains('╰'),
        "the box has **rounded** corners:\n{text}"
    );
    assert!(!text.contains('┌'), "no square corners:\n{text}");
}

// ---------- the `bar` input style (ADR 0035 V3) ----------

/// Row `y` as a string, trailing cells kept so a band's width is visible.
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

/// Asserts that the band covers row `y` from column 0 to `width`, by **display column**: a wide
/// glyph's own cell is read (and counted as its two columns), so ratatui's skip cell behind it
/// — which the terminal paints as part of the glyph — is never mistaken for a hole.
fn assert_band_across(terminal: &Terminal<TestBackend>, y: u16, band: Option<Color>, width: u16) {
    use unicode_width::UnicodeWidthStr;
    let buffer = terminal.backend().buffer();
    let mut col = 0usize;
    while col < width as usize {
        let symbol = buffer
            .cell((col as u16, y))
            .map(|cell| cell.symbol())
            .unwrap_or(" ");
        assert_eq!(
            cell_style(terminal, col as u16, y).bg,
            band,
            "row {y} column {col} is outside the band: {:?}",
            row(terminal, y)
        );
        col += UnicodeWidthStr::width(symbol).max(1);
    }
    assert_eq!(
        col,
        width as usize,
        "the band does not reach the last column: {:?}",
        row(terminal, y)
    );
}

/// ADR 0035 V3: `tui.input_style = "bar"` is one background band per input row with the accent
/// rail on the left — no border, no rules — and a multi-line input grows the band by its rows.
#[test]
fn the_bar_input_style_draws_a_full_width_band_with_the_rail() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    app.input_style = InputStyle::Bar;
    app.input = "first\nsecond".into();
    let terminal = render(&mut app, 20, 8);
    let text = screen(&terminal);
    let band = app.theme.input_bar.bg.expect("the band's background");
    let first = row_of(&terminal, "▌ first").expect("the first band row");
    let second = row_of(&terminal, "▌ second").expect("the second band row");
    assert_eq!(second, first + 1, "one band row per input row:\n{text}");
    for y in [first as u16, second as u16] {
        let line = row(&terminal, y);
        // The band's own rows carry no rule, border or box corner anywhere.
        assert!(
            !line.contains(['─', '│', '╭', '╰']),
            "a band row has no rule or border: {line:?}"
        );
        assert_eq!(
            cell_style(&terminal, 0, y).fg,
            app.theme.notice_label.fg,
            "the rail is the accent: row {y}"
        );
        // The band covers the whole row, the trailing cells included.
        assert_band_across(&terminal, y, Some(band), 20);
        // The text after the rail keeps the main level, not the accent.
        assert_eq!(
            cell_style(&terminal, 2, y).fg,
            app.theme.input_bar.fg,
            "the text is the band's own foreground: row {y}"
        );
    }
    // A CJK input wraps by display width, and every row of the wrap is banded the same way.
    let mut wide = App::new("m", "s");
    wide.input_style = InputStyle::Bar;
    wide.input = "中文中文中文".into();
    let terminal = render(&mut wide, 12, 8);
    // Inner width 10: five 2-cell characters, then the sixth.
    let first = row_of(&terminal, "▌ 中").expect("the first band row");
    let wrapped = row(&terminal, (first + 1) as u16);
    assert!(
        wrapped.starts_with("▌ ") && wrapped.contains('文'),
        "the band grew a row for the wrapped tail: {wrapped:?}"
    );
    assert_band_across(&terminal, (first + 1) as u16, wide.theme.input_bar.bg, 12);
}

/// The band reaches the terminal's last column at both widths the brief pins (80 and 120), and
/// its half-cell pads (ADR 0035/4) do too: the rail's own quadrant (`▖` above, `▘`
/// below, in the accent) then `▄`/`▀` in the band's color across the rest.
#[test]
fn the_bar_band_fills_the_terminal_width() {
    use radian_tui::render::InputStyle;
    for width in [80u16, 120] {
        let mut app = App::new("m", "s");
        app.input_style = InputStyle::Bar;
        app.input = "wide".into();
        let terminal = render(&mut app, width, 8);
        let y = row_of(&terminal, "▌ wide").expect("the band") as u16;
        let band = app.theme.input_bar.bg;
        assert_band_across(&terminal, y, band, width);
        // ADR 0035: the rail continues into the pad rows as the half-cell-wide
        // quadrants, so it is as tall as the band; the rest of the row is the band's half cell.
        assert_eq!(
            row(&terminal, y - 1),
            format!("▖{}", "▄".repeat(width as usize - 1)),
            "{width}"
        );
        assert_eq!(
            row(&terminal, y + 1),
            format!("▘{}", "▀".repeat(width as usize - 1)),
            "{width}"
        );
        for (y, rail, glyph) in [(y - 1, "▖", "▄"), (y + 1, "▘", "▀")] {
            let rail_style = cell_style(&terminal, 0, y);
            assert_eq!(rail_style.fg, app.theme.notice_label.fg, "the {rail} rail");
            assert_eq!(
                rail_style.bg,
                Some(Color::Reset),
                "the {rail} rail's cell keeps the terminal background"
            );
            for col in [1u16, width - 1] {
                let style = cell_style(&terminal, col, y);
                assert_eq!(style.fg, band, "the {glyph} pad's color at {col}");
                assert_eq!(style.bg, Some(Color::Reset), "the {glyph} pad's background");
            }
        }
        // The text rows' rail is the same accent in the same column, and the cell after it is
        // the band's own (the one cell of padding before the text at `BAR_LEAD`).
        assert_eq!(cell_style(&terminal, 0, y).fg, app.theme.notice_label.fg);
        assert_eq!(cell_style(&terminal, 1, y).fg, app.theme.input_bar.fg);
    }
}

/// ADR 0035 V3 / decision 1: the status row above the band is **always** one row —
/// blank while idle, `<spinner>  Working · 12s · esc to interrupt` while a turn runs — so the
/// band and the transcript never move when a turn starts or ends.
#[test]
fn the_bar_status_row_never_moves_the_band_or_the_transcript() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    app.input_style = InputStyle::Bar;
    app.spinner_kind = radian_tui::spinner::Kind::Arc;
    app.set_now(app.session.spinner_origin);
    app.input = "hello".into();
    app.push_message(&Message::user("a question"));

    // Wide enough that the whole status row fits: it is asserted on, not clipped.
    let idle = render(&mut app, 80, 10);
    let band_row = row_of(&idle, "▌ hello").expect("the band");
    let transcript_row = row_of(&idle, "a question").expect("the transcript");
    // The band's half-cell pads are always there (ADR 0035/4): the rail's `▖`
    // above, `▘` below, then `▄`/`▀` for the rest of the row.
    assert!(row(&idle, (band_row - 1) as u16).starts_with("▖▄"));
    assert!(row(&idle, (band_row + 1) as u16).starts_with("▘▀"));
    // The status row is one above the top pad, and blank (but there) while idle.
    assert_eq!(row(&idle, (band_row - 2) as u16).trim_end(), "");
    assert!(!screen(&idle).contains("Working"));

    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    app.session
        .timer
        .request_started(app.session.spinner_origin);
    app.set_now(app.session.spinner_origin + std::time::Duration::from_secs(12));
    let running = render(&mut app, 80, 10);
    assert_eq!(
        row_of(&running, "▌ hello"),
        Some(band_row),
        "the band must not move when the turn starts"
    );
    assert_eq!(
        row_of(&running, "a question"),
        Some(transcript_row),
        "the transcript must not move either"
    );
    let status = row(&running, (band_row - 2) as u16);
    assert!(
        status.contains("◜ Working · 12s · esc to interrupt"),
        "the live status row: {status:?}"
    );
    // ADR 0035: the dot is in the rail's column (0) — the same vertical line as the
    // rail — one cell before the text, which starts at the text column.
    let cells: Vec<char> = status.chars().collect();
    assert_eq!(cells[0], '◜', "the dot's column: {status:?}");
    assert_eq!(
        cells[1], ' ',
        "one cell between the dot and the text: {status:?}"
    );
    assert_eq!(cells[2], 'W', "the text column: {status:?}");
    assert_eq!(
        cell_style(&running, 0, (band_row - 2) as u16).fg,
        app.theme.notice_label.fg,
        "the dot takes the accent"
    );

    // The turn ends: the row is blank again and nothing moved.
    app.session.projection.set_turn(radian_tui::TurnState::Idle);
    let after = render(&mut app, 80, 10);
    assert_eq!(row_of(&after, "▌ hello"), Some(band_row));
    assert_eq!(row_of(&after, "a question"), Some(transcript_row));
    assert_eq!(row(&after, (band_row - 2) as u16).trim_end(), "");

    // A running `!` is a status but not a **turn**: its row carries the state without the
    // turn's interrupt hint (Esc does not cancel a `!` through this path).
    app.session.bash_running = Some("sleep 1".to_string());
    let terminal = render(&mut app, 80, 10);
    let status = row(&terminal, (band_row - 2) as u16);
    assert!(status.contains("Running a command"), "{status:?}");
    assert!(
        !status.contains("esc to interrupt"),
        "no turn is running: {status:?}"
    );
    app.session.bash_running = None;
}

/// ADR 0035 V3 + revision 3: `bar` spends one row on the status row, one per input row and the
/// band's two half-cell pad rows (`▄` above, `▀` below) — no rule or border rows and no slack.
/// The bottom pad is the input area's last row (the `classic` status bar is directly below it),
/// and a two-row input grows the band by exactly one row.
#[test]
fn the_bar_input_takes_the_status_row_the_pad_rows_and_its_text_rows() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.input_style = InputStyle::Bar;
    app.input = "hello".into();
    // Height 8, the one-row `classic` status bar at row 7: the status row is 3, the top pad 4,
    // the band 5, the bottom pad 6.
    let terminal = render(&mut app, 20, 8);
    assert_eq!(
        row_of(&terminal, "▌ hello"),
        Some(5),
        "the band sits between the two pad rows:\n{}",
        screen(&terminal)
    );
    assert_eq!(row(&terminal, 3).trim_end(), "", "the blank status row");
    assert!(
        row(&terminal, 4).starts_with('▖'),
        "the top pad's rail cell"
    );
    assert!(
        row(&terminal, 6).starts_with('▘'),
        "the bottom pad's rail cell"
    );

    app.input = "one\ntwo".into();
    let terminal = render(&mut app, 20, 8);
    assert_eq!(
        row_of(&terminal, "▌ one"),
        Some(4),
        "the status row moved up"
    );
    assert_eq!(row_of(&terminal, "▌ two"), Some(5));
    assert!(
        row(&terminal, 3).starts_with('▖'),
        "the top pad's rail cell"
    );
    assert!(
        row(&terminal, 6).starts_with('▘'),
        "the bottom pad's rail cell"
    );
}

/// `tui.footer.glyphs = "ascii"` reaches the rail and the status spinner too (ADR 0027
/// revision 4: the unicode glyphs are only used when the font set has them).
#[test]
fn the_bar_rail_and_spinner_fall_back_to_ascii() {
    use radian_tui::render::InputStyle;
    let mut app = App::new("m", "s");
    app.input_style = InputStyle::Bar;
    app.input = "hi".into();
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".to_string()),
        ..Default::default()
    });
    app.set_now(app.session.spinner_origin);
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let text = screen(&render(&mut app, 40, 10));
    assert!(text.contains("| hi"), "the ascii rail:\n{text}");
    assert!(!text.contains('▌'), "no unicode rail:\n{text}");
    // ADR 0035: the `ascii` tier gets no half-cell band padding.
    assert!(
        !text.contains('▄') && !text.contains('▀'),
        "no band padding under ascii:\n{text}"
    );
    assert!(
        text.contains("* Working · esc to interrupt"),
        "the ascii pulse glyph: \n{text}"
    );
}

/// ADR 0035: the `ascii` glyph tier gets no half-cell padding — a user
/// block is just its text rows, so two of them sit on consecutive lines (the container's gap
/// between them is still zero, as it is a user block on each side).
#[test]
fn the_ascii_tier_has_no_band_padding() {
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
    // No pad rows: the two user blocks are two rows in a row.
    assert_eq!(row_of(&terminal, "> hello"), Some(0), "{text}");
    assert_eq!(row_of(&terminal, "> again"), Some(1), "{text}");
}

/// ADR 0035 (replacing revision 3 decision 3): thinking has **one blank line on both
/// sides** — folded or expanded, and whether the block below it is the reply or a tool card. The
/// gap above it stays one line (here the user block's bottom pad).
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
    let user = row_of(&terminal, "> q").expect("the user text");
    let thinking = row_of(&terminal, "thinking (1 lines)").expect("the folded channel");
    let answer = row_of(&terminal, "the answer").expect("the reply");
    assert_eq!(
        thinking,
        user + 2,
        "the user block's bottom pad is the one-line gap above thinking:\n{text}"
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

    // Expanded, and directly against a tool card: the same single line. The entries are pushed
    // directly because `push_message` always puts an (empty) assistant entry between them — a
    // block of its own, which is what makes a text-less call show two blank rows in a real
    // transcript, not this rule. The unit test in `blocks.rs` pins the pair rule itself.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Expanded;
    app.session.entries.push(Entry::new(
        EntryKind::Reasoning,
        "first thought\nsecond thought",
    ));
    let card = radian_tui::tools::ToolCard::new(
        "c1",
        "shell",
        serde_json::json!({"command": "make test"}),
    );
    let mut call = Entry::new(EntryKind::ToolCall, "-> shell: make test");
    call.tool = Some(Box::new(card));
    call.card = radian_tui::blocks::Card::Solo;
    app.session.entries.push(call);
    let terminal = render(&mut app, 40, 12);
    let text = screen(&terminal);
    let first = row_of(&terminal, "first thought").expect("the expanded thinking body");
    let body = row_of(&terminal, "second thought").expect("the second thinking line");
    assert_eq!(
        body,
        first + 1,
        "the body's own lines are adjacent:\n{text}"
    );
    // The card's own block starts at its top border; the head text is inside it.
    let top = row_of(&terminal, "┌").expect("the card's top border");
    assert_eq!(
        top,
        body + 2,
        "one blank line between expanded thinking and the tool card:\n{text}"
    );
    assert_eq!(
        row(&terminal, (body + 1) as u16).trim_end(),
        "",
        "and that line is empty:\n{text}"
    );
    assert!(row(&terminal, top as u16).contains('┌'), "{text}");
}

/// ADR 0027 item 2 / ADR 0035: the `unicode` tier's tool
/// success glyph is `✓` (not `+`). A regression to `+` fails here — the review found the change
/// untested, so the rendered glyph is pinned.
#[test]
fn the_unicode_tool_success_glyph_is_a_check_mark() {
    assert_eq!(
        radian_tui::footer::Glyphs::Unicode.tool_status()[1],
        "✓",
        "the unicode tier's success glyph"
    );
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
        text.contains("✓ shell"),
        "a completed card draws the check mark:\n{text}"
    );
    assert!(!text.contains("+ shell"), "never the old `+`:\n{text}");
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
    let (total, _) =
        radian_tui::render::visible_lines(&mut app.session.entries, EXPANDED, 40, 0, 0, &app.theme);
    app.set_viewport(u16::try_from(total).unwrap(), total);
    // Select "selection" on the block's text line (a user block is pad / text / pad): body
    // columns 6..15 → screen columns 8..17.
    let selection = Selection {
        anchor: locate(&app.session.entries, EXPANDED, 40, 1, 8).expect("anchor"),
        head: locate(&app.session.entries, EXPANDED, 40, 1, 17).expect("head"),
    };
    app.session.selection = Some(selection);
    // Tall enough that the whole three-row user block is on screen (the `bar` input is four
    // rows with the half-cell pads, ADR 0035).
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
    let (total, _) =
        radian_tui::render::visible_lines(&mut app.session.entries, EXPANDED, 40, 0, 0, &app.theme);
    app.set_viewport(u16::try_from(total).unwrap(), total);
    app.session.selection = Some(Selection {
        anchor: locate(&app.session.entries, EXPANDED, 40, 1, 8).expect("anchor"),
        head: locate(&app.session.entries, EXPANDED, 40, 1, 17).expect("head"),
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
    let (total, _) =
        radian_tui::render::visible_lines(&mut app.session.entries, EXPANDED, 40, 0, 0, &app.theme);
    app.set_viewport(u16::try_from(total).unwrap(), total);
    let at = locate(&app.session.entries, EXPANDED, 40, 1, 8).expect("anchor");
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
    // Find the long line's row: it starts with 79 'x' and ends with '…' on the same row.
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
        if text.starts_with("xxxx") {
            found = Some((row, text));
            break;
        }
    }
    let (row, text) = found.expect("the long code line is on screen");
    assert_eq!(text.chars().filter(|&c| c == 'x').count(), 79, "{text:?}");
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
        next.starts_with("second"),
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
    // The card's head is the status line (ADR 0028): `shell: <title>`, with the
    // command and the output as its sections.
    let ok = live.footer_glyphs().tool_status()[1];
    assert!(
        live_text.contains(&format!("{ok} shell: make test")),
        "{live_text}"
    );
    assert!(live_text.contains("$ make test"), "{live_text}");
    assert!(
        !live_text.contains("Command"),
        "no command divider (ADR 0028): {live_text}"
    );
    assert!(live_text.contains("├── Output"), "{live_text}");
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
    // errors (the default): a success folds to the preview + one hint.
    assert_eq!(app.tool_display.fold, ToolFold::Errors);
    let _ = render(&mut app, 60, 30);
    let folded = app.session.total_lines;
    // The card's frame (top, the Command divider, the bottom) is constant across the folds.
    assert!(folded < 14, "folded: {folded} lines");
    // Ctrl-O: errors -> expanded -> collapsed -> errors.
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Expanded);
    let _ = render(&mut app, 60, 30);
    let expanded = app.session.total_lines;
    assert_eq!(
        expanded,
        folded + 14,
        "20 lines instead of 5 preview + hint"
    );
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Collapsed);
    let _ = render(&mut app, 60, 30);
    // `collapsed` keeps the header only: the five preview lines `errors` showed are gone, and
    // with them the Output divider (a folded hint is not a section, ADR 0028).
    assert_eq!(
        app.session.total_lines,
        folded - 6,
        "collapsed shows the header only"
    );
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.tool_display.fold, ToolFold::Errors);
}

/// The `git_diff` body colors the four line kinds with the theme's own roles (ADR 0028 R4 +
/// ADR 0029): added, removed, context and the hunk/file-header structure.
#[test]
fn a_git_diff_body_colors_its_line_kinds() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "git_diff".into(),
            arguments: serde_json::json!({}),
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
    assert_eq!(style("+new line").fg, theme.diff_added.fg, "added");
    assert_eq!(style("-old line").fg, theme.diff_removed.fg, "removed");
    assert_eq!(style("context").fg, theme.diff_context.fg, "context");
    assert_eq!(style("@@").fg, theme.diff_hunk.fg, "@@ is structure");
    assert_eq!(
        style("diff --git").fg,
        theme.diff_hunk.fg,
        "the file header is structure"
    );
    // The head's counts come from the same text.
    let text = screen(&terminal);
    assert!(text.contains("git_diff: 1 files +1/-1"), "{text}");
}

/// A `patch` card shows the replacement: the head's +N/-M and the `-old`/`+new` body, whose
/// lines carry the diff roles. The body **is** the entry's text, so it copies as shown.
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
    assert!(text.contains("patch: src/main.rs +1/-2"), "{text}");
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

/// A `write` card: the head counts, the body is the content (no `+` markers), and a CJK path
/// renders.
#[test]
fn a_write_card_shows_the_content_without_markers() {
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
    assert!(squeezed.contains("write:笔记.md(19bytes,2lines)"), "{text}");
    assert!(squeezed.contains("第一行"), "{text}");
    assert!(squeezed.contains("第二行"), "{text}");
    let body = style_of(&terminal, "第").expect("the content");
    assert_eq!(
        body.fg, app.theme.tool_result.fg,
        "no diff role for a write body"
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
                    name: "list".into(),
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
        agent_core::compaction::compacted_history("the earlier work, summarized", &[], &[])
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
    // The status glyphs main's footer draws (running, success, failure).
    let status = app.footer_glyphs().tool_status();
    // The frame's rows are built, not typed out: an 80-column border is 78 cells wide.
    let top = format!("┌{}┐", "─".repeat(78));
    let bottom = format!("└{}┘", "─".repeat(78));
    // `├── label ──…┤` is 80 cells: 4 + label + 1 + fill + 1.
    let divider = |label: &str| format!("├── {label} {}┤", "─".repeat(74 - label.chars().count()));
    let row = |text: &str| {
        format!(
            "│ {text}{}│",
            " ".repeat(77usize.saturating_sub(text.chars().count()))
        )
    };
    let golden: Vec<String> = vec![
        // ADR 0029 made `user` a foreground role, so the user block's
        // two pad rows are plain blank rows now (no band).
        "".to_string(),
        "> explain the failure".to_string(), // the user text
        "".to_string(),
        "› thinking (3 lines)".to_string(), // the folded channel: the unicode fold glyph (ADR 0027)
        "".to_string(), // ADR 0035: one blank line between thinking and the block below
        "  Looking into it.".to_string(), // the reply
        "".to_string(), // the gap
        top.clone(),    // the shell card's frame (ADR 0028)
        row(&format!("{} shell: make test", status[2])), // the head status line
        row("$ make test"), // the command (no divider: part of the call)
        divider("Error"), // the output section's label (the call failed)
        row("$ make test"), // the output
        row("exit code: 2"),
        row("--- stdout ---"),
        row("boom"),
        row("⟦exit 2⟧"), // the meta row, `⟦ ⟧` (revision 3 rule 3)
        bottom.clone(),  // the card's bottom border
        "".to_string(),  // the gap
        top.clone(),     // the list card
        row(&format!("{} list: src (2 entries)", status[1])),
        divider("Output"),
        row("d 4.0K src"), // the aligned table
        row("f 512 B b.md"),
        bottom.clone(),
        "".to_string(), // the gap
        "* profile: workspace · sandbox macos-seatbelt".to_string(),
        "".to_string(), // the gap above the rule line
        "── context compacted ──".to_string(),
        "".to_string(), // the gap below it
    ];
    assert_eq!(rows[..golden.len()], golden[..], "{rows:#?}");
    // The bands: ADR 0029 made `user` a foreground role, so the user
    // block has no band — its two pad rows are plain blank rows. The failed card's frame and
    // every row inside it still carry the card's band; a gap row carries none.
    assert_eq!(app.theme.user.bg, None, "the user role has no background");
    assert_eq!(
        cell_style(&terminal, 0, 1).bg,
        Some(Color::Reset),
        "the user text row is unbanded"
    );
    for y in [0u16, 2] {
        let style = cell_style(&terminal, 79, y);
        assert_eq!(style.fg, Some(Color::Reset), "no pad color at {y}");
        assert_eq!(
            style.bg,
            Some(Color::Reset),
            "the pad has no background at {y}"
        );
    }
    let bg = |x: u16, y: u16| buffer.cell((x, y)).map(|c| c.bg);
    // ADR 0035's blank line moved everything below thinking down by one: the reply is
    // row 5, the gap row 6, the failed card 7..=16 and the list card starts at 18.
    let failed = bg(0, 7);
    for y in 7..=16u16 {
        assert_eq!(
            bg(0, y),
            failed,
            "the failed card's row {y} carries the band"
        );
        assert_eq!(bg(79, y), failed, "…to the row's end");
    }
    assert_ne!(failed, bg(0, 5), "the assistant row has no card band");
    assert_eq!(bg(0, 6), bg(0, 5), "a gap row is not banded");
    assert_ne!(failed, bg(0, 18), "the successful card's band differs");
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

/// A `list` card shows an aligned table, not the tool's tab-separated text (owner hand test:
/// `dir512crates` ran together because a tab occupies no column).
#[test]
fn a_list_card_shows_an_aligned_table() {
    let mut app = App::new("m", "s");
    app.push_message(&assistant_with(
        "",
        "",
        false,
        vec![ToolCall {
            id: "c1".into(),
            name: "list".into(),
            arguments: serde_json::json!({"path": "src"}),
        }],
    ));
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text(
                "file\t512\tb.md\ndir\t4096\tsrc\nfile\t1693\tCargo.toml\n",
            )],
            structured: Some(serde_json::json!({
                "path": "src", "entries": 3, "total_entries": 3, "truncated": false
            })),
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    app.tool_display.fold = radian_tui::tools::ToolFold::Expanded;
    let text = screen(&render(&mut app, 70, 14));
    assert!(text.contains("list: src (3 entries)"), "{text}");
    assert!(text.contains("d ") && text.contains("src"), "{text}");
    assert!(text.contains("4.0K"), "a human-readable size: {text}");
    assert!(
        !text.contains("dir\t4096"),
        "no raw tab-separated text: {text}"
    );
    assert!(!text.contains('\t'), "no tab reaches the screen: {text}");
    // Directories first.
    let dir_row = text.find("d ").expect("a dir row");
    let file_row = text.find("f ").expect("a file row");
    assert!(dir_row < file_row, "the directory sorts first: {text}");
}

/// The list-like and memory renderers' heads.
#[test]
fn the_list_and_memory_cards_have_their_own_heads() {
    for (name, arguments, structured, expected) in [
        (
            "git_status",
            serde_json::json!({}),
            serde_json::json!({"entries": 2}),
            "git_status: 2 entries",
        ),
        (
            "list",
            serde_json::json!({"path": "src"}),
            serde_json::json!({"path": "src", "entries": 3, "total_entries": 3}),
            "list: src (3 entries)",
        ),
        (
            "ask_user",
            serde_json::json!({"questions": [{"id": "q1"}, {"id": "q2"}]}),
            serde_json::json!({"request_id": "r", "answers": []}),
            // ADR 0024 R1: the call is a boxed block, not the plain head.
            "Ask 2 questions",
        ),
        (
            "memory_get",
            serde_json::json!({"id": "mem-7"}),
            serde_json::json!({}),
            "memory_get: mem-7",
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
    use agent_core::compaction::{RetainedUserMessage, compacted_history};
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
    use agent_core::compaction::compacted_history;
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
        !text.contains("read: a.rs"),
        "not the built-in read card: {text}"
    );
    // Same for a `shell` and a `list` from a server.
    let text = screen(&render(
        &mut card_for("x__shell", serde_json::json!({"cmd": "ls"})),
        70,
        12,
    ));
    assert!(text.contains("x__shell"), "{text}");
    assert!(!text.contains("shell: ls"), "{text}");
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
    assert!(text.contains("read: a.rs (lines 4..5)"), "{text}");
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
    // The empty assistant entry contributes nothing (its blank line is stripped, rule 1) and
    // the container's gap before the card is one line. The card (ADR 0028) is the
    // top border + the head + the body + the bottom border, plus the Output divider only when
    // a real output section is shown: 20 body lines expanded, 5 preview + 1 hint + the
    // divider under `errors`, a single `[20 lines]` hint and no divider under `collapsed`.
    assert_eq!(counts, vec![25, 11, 5], "expanded / errors / collapsed");
    // A failure: the same count in all three, and the body really visible.
    for fold in [ToolFold::Expanded, ToolFold::Errors, ToolFold::Collapsed] {
        let mut app = app_with(true);
        app.tool_display.fold = fold;
        let text = screen(&render(&mut app, 60, 40));
        assert!(text.contains("line 1 "), "{fold:?}: {text}");
        assert!(text.contains("line 20"), "{fold:?}");
        assert!(!text.contains("more lines"), "{fold:?}: nothing folded");
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
    assert!(text.contains("› thinking (2 lines)"), "{text}");
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
    assert!(!text.contains("› thinking"), "{text}");

    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let text = screen(&render(&mut app, 60, 10));
    assert!(text.contains("› thinking (2 lines)"), "{text}");
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
        collapsed + 2,
        "three thinking lines become one placeholder"
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

/// The user block's band with the half-cell padding (ADR 0035): the text row paints
/// the `user` band across the whole row (the marker, the text, the trailing whitespace); the
/// rows above and below are half a cell of it (`▄`/`▀`) in the band's **color** as the glyph's
/// foreground, on the terminal's default background. Pinned at 80 and 120 columns.
#[test]
fn a_user_block_pads_its_band_with_half_cells_at_80_and_120() {
    let band = Color::Rgb(0x18, 0x18, 0x20);
    for width in [80u16, 120] {
        let mut app = App::new("m", "s");
        app.theme.user = ratatui::style::Style::default().bg(band);
        app.push_message(&Message::user("hello band"));
        // Tall enough for the whole three-row block plus the input's pads and the footer.
        let terminal = render(&mut app, width, 12);
        // Row 0 is the band's top half cell: `▄` across the whole row.
        assert_eq!(row(&terminal, 0), "▄".repeat(width as usize), "{width}");
        // Row 1 is the text row: the band covers it end to end.
        assert!(row(&terminal, 1).contains("hello band"));
        // Row 2 is the bottom half cell: `▀` across the whole row.
        assert_eq!(row(&terminal, 2), "▀".repeat(width as usize), "{width}");
        for col in [0u16, width - 1] {
            for (y, glyph) in [(0u16, "▄"), (2u16, "▀")] {
                let style = cell_style(&terminal, col, y);
                assert_eq!(
                    style.fg,
                    Some(band),
                    "the {glyph} pad's color at ({col}, {y})"
                );
                assert_eq!(
                    style.bg,
                    Some(Color::Reset),
                    "the {glyph} pad has no background"
                );
            }
            // The text row's background is the band, not a half cell.
            assert_eq!(cell_style(&terminal, col, 1).bg, Some(band));
        }
        // The row below the block is not painted.
        let below = terminal.backend().buffer().cell((0, 3)).expect("on screen");
        assert_ne!(below.bg, band);
    }
}

/// A tool card's band follows its state (ADR 0028 R4): running / success / failure, and the
/// result entry carries the same two.
#[test]
fn a_tool_card_paints_the_state_band() {
    for (result, expected) in [
        (None, Color::Rgb(52, 56, 58)),
        (Some(false), Color::Rgb(37, 65, 49)),
        (Some(true), Color::Rgb(91, 40, 42)),
    ] {
        let mut app = App::new("m", "s");
        let mut call = Entry::new(EntryKind::ToolCall, "shell({\"command\": \"ls\"})");
        call.result = result;
        app.session.entries.push(call);
        let terminal = render(&mut app, 40, 10);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((0, 0)).expect("on screen").bg,
            expected,
            "the head row's band for {result:?}"
        );
        assert_eq!(
            buffer.cell((39, 0)).expect("on screen").bg,
            expected,
            "the band reaches the row's end"
        );
    }

    // A tool result paints the card body the same way.
    let mut app = App::new("m", "s");
    classic_footer(&mut app);
    let mut result = Entry::new(EntryKind::ToolResult, "line one\nline two");
    result.is_error = true;
    app.session.entries.push(result);
    let terminal = render(&mut app, 40, 10);
    let buffer = terminal.backend().buffer();
    for row in 0..2u16 {
        for col in 0..40u16 {
            assert_eq!(
                buffer.cell((col, row)).expect("on screen").bg,
                Color::Rgb(91, 40, 42),
                "the error band covers row {row} col {col}"
            );
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
/// choices through the shared list, at 80/120 columns and in the ascii tier. The cursor's
/// default is the fail-safe `UntrustHere`.
#[test]
fn the_trust_panel_snapshot_at_80_120_and_ascii() {
    let mut app = App::new("m", "s");
    app.push_trust_modal(std::path::PathBuf::from("/work/example"));

    let terminal = render(&mut app, 80, 24);
    let text = screen(&terminal);
    assert!(text.contains("╭─ project trust "), "{text}");
    assert!(
        text.contains("project resources found in /work/example"),
        "{text}"
    );
    assert!(
        text.contains("❯ Do not trust this directory (/work/example)"),
        "the fail-safe default: {text}"
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
    assert!(text.contains("> Do not trust this directory"), "{text}");
    assert!(!text.contains('╭'), "{text}");
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
        text.contains("⏎ select"),
        "the keys are in the bottom border:\n{text}"
    );
    assert!(text.contains("esc canc"), "{text}");
    assert!(text.contains("Ask · session"), "{text}");
}

/// ADR 0024 rule 4: the Ask panel title is `Ask` alone when the asker is this
/// process's own session, and `Ask · <origin>` only when another session (an ADR 0030 task
/// agent) is asking. A TUI process has one session today, so `origin == own session` is the
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
/// ("the questions are in the panel below"); once the answer arrives the **same** entry draws
/// the full closed block, and the result entry stays blank, so the ask is never drawn twice.
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

    // Pending: one line, no questions or options, and only one "Ask" line.
    let pending = screen(&render(&mut app, 70, 16));
    assert!(
        pending.contains("-> Ask 2 questions · answering below"),
        "the pending one-liner: {pending}"
    );
    assert!(
        !pending.contains("Pick one") && !pending.contains("Option A"),
        "the pending line carries no questions or options: {pending}"
    );
    assert_eq!(
        pending.matches("Ask 2 questions").count(),
        1,
        "only one Ask line while pending: {pending}"
    );

    // Answered: the same call entry becomes the full closed block (its `->` marker kept), and
    // the result entry draws nothing, so there is exactly one block.
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
        answered.contains("->╭─ Ask 2 questions"),
        "the same entry draws the block, keeping its marker: {answered}"
    );
    assert!(answered.contains("Pick one"), "{answered}");
    assert!(answered.contains("☑ Option A"), "{answered}");
    assert!(
        !answered.contains("answering below"),
        "the pending line is gone: {answered}"
    );
    assert_eq!(
        answered.matches("╭─ Ask 2 questions").count(),
        1,
        "exactly one block, not two: {answered}"
    );
}

/// ADR 0024 rule 2: a declined ask shows its status line inside the same closed
/// block.
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
        declined.contains("->╭─ Ask 1 question"),
        "the declined block keeps the call marker: {declined}"
    );
    assert!(
        declined.contains("user declined to answer"),
        "the status line stays inside the box: {declined}"
    );
    assert!(
        !declined.contains("answering below"),
        "the pending line is gone: {declined}"
    );
    assert_eq!(
        declined.matches("╭─ Ask 1 question").count(),
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

/// The chosen option's marker is drawn in its own span and is never bold: the owner's font
/// draws a glyph missing from its bold face at a smaller size (ADR 0024 note).
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

    let marker = style_of(&terminal, "☑").expect("the chosen marker cell");
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

/// Single, multi and `Other` all draw the same checkbox pair (ADR 0024 note): the
/// old circle pair is gone.
#[test]
fn the_ask_panel_uses_one_checkbox_pair_for_single_multi_and_other() {
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
    assert!(text.contains("☑ Alpha"), "the chosen single option: {text}");
    assert!(
        text.contains("☐ Beta"),
        "the unchosen single option: {text}"
    );
    assert!(text.contains("☐ Other (type your own)"), "{text}");
    assert!(
        !text.contains('○')
            && !text.contains('◉')
            && !text.contains("(*)")
            && !text.contains("( )"),
        "no circle markers: {text}"
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
    assert!(text.contains("☑ Alpha"), "{text}");
    assert!(text.contains("☐ Beta"), "{text}");
    assert!(!text.contains('○') && !text.contains('◉'), "{text}");

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
        text.contains("☑ Other"),
        "the chosen Other row uses the checked box, not a circle: {text}"
    );
}

/// The panel and the transcript block draw the markers from the same source, so both show the
/// checkbox pair (ADR 0024 note).
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
    assert!(panel.contains('☑') && panel.contains('☐'), "{panel}");

    let mut card = radian_tui::tools::ToolCard::new(
        "c1",
        "ask_user",
        serde_json::json!({"questions":[{"id":"q","kind":"single","options":[
            {"id":"a","label":"Alpha"},{"id":"b","label":"Beta"}]}]}),
    );
    card.structured = Some(serde_json::json!({"answers":[{"id":"q","selected":["a"]}]}));
    let mut entry = radian_tui::app::Entry::new(radian_tui::app::EntryKind::ToolResult, "result");
    entry.tool = Some(Box::new(card));
    let block = radian_tui::blocks::layout(&entry, &[], EXPANDED, 60);
    let block_text: String = block
        .iter()
        .map(|line| line.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        block_text.contains('☑') && block_text.contains('☐'),
        "the transcript block uses the same pair: {block_text}"
    );
    assert!(
        !block_text.contains('○') && !block_text.contains('◉'),
        "{block_text}"
    );
}
