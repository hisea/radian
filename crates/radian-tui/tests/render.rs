//! Rendering tests on `TestBackend`: reasoning and assistant text are distinguishable,
//! an interrupted prefix is marked, and tool calls are shown.

use agent_core::{AgentEvent, ContentPart, Message, ToolCall, ToolResult};
use radian_coding::QuestionKind;
use radian_host::{PendingRequest, PendingView};
use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::projection::StreamKind;
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

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
            // The first character of the match is at display column `col`, but wide cells
            // shift columns; for ASCII markers they coincide.
            let x = area.left() + col as u16;
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
        reasoning
            .add_modifier
            .contains(ratatui::style::Modifier::ITALIC),
        "reasoning is italic: {reasoning:?}"
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

#[test]
fn the_status_line_shows_model_and_turn_state() {
    let mut app = App::new("my-model", "s");
    // The turn state is a session fact: the event sink writes it to the projection
    // (ADR 0023 U5), so the test writes it there too.
    app.projection.set_turn(radian_tui::TurnState::Running);
    app.entries.push(Entry::new(EntryKind::User, "hi"));
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(text.contains("my-model"), "{text}");
    assert!(text.contains("running"), "{text}");
}

#[test]
fn live_deltas_are_replaced_by_the_committed_message_not_duplicated() {
    let mut app = App::new("m", "s");
    let projection = radian_tui::projection::SessionProjection::new();
    stream(&mut app, &projection, StreamKind::Assistant, "hel");
    stream(&mut app, &projection, StreamKind::Assistant, "lo");
    assert_eq!(app.entries.len(), 1);
    assert_eq!(app.entries[0].text, "hello");
    app.apply_event(&AgentEvent::TranscriptAppended {
        index: 1,
        message: assistant_with("hello", "", false, vec![]),
    });
    let assistant: Vec<&Entry> = app
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
    app.entries.push(Entry::new(
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
        app.entries[0].text, "look\n\n<file path=\"notes.txt\">\nhello\nworld\n</file>",
        "the entry text is the full message"
    );
}

/// The `@` completion list is drawn above the input box and is clipped to the terminal width.
#[test]
fn the_completion_list_sits_above_the_input_and_truncates() {
    let mut app = App::new("m", "s");
    app.input = "@no".into();
    app.completion_set(vec!["notes.txt".into(), "notes.md".into()]);
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
    app.completion_set(vec!["a-very-long-candidate-name.txt".into()]);
    let text = screen(&render(&mut app, 12, 10));
    assert!(
        !text.contains("a-very-long-candidate-name.txt"),
        "the row must be clipped, not wrapped:\n{text}"
    );
    assert!(text.contains("a-very-long"), "{text}");
}

/// `completion_rows` itself truncates, so the rule is testable without a terminal that
/// clips: a row longer than the width is cut to exactly the width.
#[test]
fn completion_rows_truncates_to_the_width() {
    let mut app = App::new("m", "s");
    app.completion_set(vec!["a-very-long-name.txt".into()]);
    assert_eq!(render::completion_rows(&app, 6), vec!["a-very".to_string()]);
}

#[test]
fn the_completion_list_shows_ten_then_more() {
    let mut app = App::new("m", "s");
    app.completion_set((0..15).map(|i| format!("file{i:02}.txt")).collect());
    let text = screen(&render(&mut app, 40, 20));
    assert!(text.contains("file09.txt"), "{text}");
    assert!(!text.contains("file10.txt"), "{text}");
    assert!(text.contains("+5 more"), "{text}");
}

// ---------- the question form (ADR 0024 Q7) ----------

fn option(id: &str, label: &str) -> radian_coding::QuestionOption {
    radian_coding::QuestionOption {
        id: id.into(),
        label: label.into(),
        description: None,
    }
}

fn form(questions: Vec<radian_coding::Question>) -> radian_tui::question::QuestionForm {
    radian_tui::question::QuestionForm::new(7, "session".into(), questions)
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
        prompt: "Pick one".into(),
        kind: radian_coding::QuestionKind::Single,
        options,
        allow_other: false,
        required: true,
    }]);
    // Move the cursor to the last option, with only five visible rows.
    form.answers[0].cursor = 11;
    let shown = text_of(&render::question_lines(&form, 40, 5));
    assert!(shown.contains("> ( ) Option 11"), "{shown}");
    assert!(
        shown.contains("above"),
        "the clipped rows are counted: {shown}"
    );
    assert!(
        !shown.contains("Option 0"),
        "the top scrolled away: {shown}"
    );
}

/// A narrow box clips its rows instead of wrapping them (ADR 0024 Q7).
#[test]
fn a_narrow_question_box_clips_instead_of_wrapping() {
    let form = form(vec![radian_coding::Question {
        id: "q".into(),
        prompt: "a prompt that is far too long for the box".into(),
        kind: radian_coding::QuestionKind::Single,
        options: vec![option("a", "an option label that is also far too long")],
        allow_other: false,
        required: false,
    }]);
    let shown = text_of(&render::question_lines(&form, 20, 10));
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
            id: "one".into(),
            prompt: "Pick one".into(),
            kind: radian_coding::QuestionKind::Single,
            options: vec![option("a", "A")],
            allow_other: false,
            required: false,
        },
        radian_coding::Question {
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
    let shown = text_of(&render::question_lines(&form, 60, 10));
    assert!(shown.contains("1. Pick one → A"), "{shown}");
    assert!(
        shown.contains("2. Say something → (not answered)"),
        "{shown}"
    );
}

/// The decline confirmation is shown, so the first `Esc` never looks like it did nothing.
#[test]
fn the_decline_confirmation_is_shown() {
    let mut form = form(vec![radian_coding::Question {
        id: "one".into(),
        prompt: "Pick one".into(),
        kind: radian_coding::QuestionKind::Text,
        options: Vec::new(),
        allow_other: false,
        required: false,
    }]);
    form.confirm_decline = true;
    let shown = text_of(&render::question_lines(&form, 60, 10));
    assert!(shown.contains("press Esc again to decline"), "{shown}");
}

/// The input area in the style of pi: a rule above and below, `> ` on the
/// first line, and continuation lines indented by two cells instead of repeating the prompt.
#[test]
fn the_input_area_has_rules_and_a_prompt_instead_of_a_box() {
    let mut app = App::new("m", "s");
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
    assert_eq!(lines[rules[0] + 1].trim_end(), "> first", "{text}");
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

/// The empty input shows the placeholder after the prompt.
#[test]
fn the_empty_input_shows_the_placeholder_after_the_prompt() {
    let mut app = App::new("m", "s");
    let text = screen(&render(&mut app, 80, 8));
    assert!(
        text.contains("> (type a message, Enter to send, Ctrl-J for a new line)"),
        "{text}"
    );
}

/// `tui.input_style = "box"` keeps the bordered input box (the pre-review shape).
#[test]
fn the_box_input_style_draws_the_bordered_box() {
    let mut app = App::new("m", "s");
    app.input = "first\nsecond".into();
    app.input_style = radian_tui::render::InputStyle::Box;
    let text = screen(&render(&mut app, 20, 8));
    assert!(text.contains(" message "), "the box title is back:\n{text}");
    assert!(
        text.contains("│first"),
        "the text starts after the border:\n{text}"
    );
    assert!(!text.contains("> first"), "{text}");
    assert!(
        text.contains("┌") && text.contains("└"),
        "the box has corners, the lines style has none:\n{text}"
    );
}

/// A transcript selection draws with the theme's `selection` role: the band lands exactly on
/// the cells `select::highlight_rows` reports, prefix cells included, and the text keeps its
/// own foreground (the patch only adds the background).
#[test]
fn a_selection_draws_the_selection_role_on_its_cells() {
    use radian_tui::blocks::ReasoningMode;
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.entries,
        ReasoningMode::Expanded,
        40,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    // Select "selection" on the block's text line (a user block is pad / text / pad): body
    // columns 6..15 → screen columns 8..17.
    let selection = Selection {
        anchor: locate(&app.entries, ReasoningMode::Expanded, 40, 1, 8).expect("anchor"),
        head: locate(&app.entries, ReasoningMode::Expanded, 40, 1, 17).expect("head"),
    };
    app.selection = Some(selection);
    let terminal = render(&mut app, 40, 6);
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
        Some(Color::Rgb(33, 59, 73)),
        "the user block's band, not the selection's"
    );
    assert_eq!(before.fg, word.fg, "the text keeps its own foreground");
}

/// The `mono` theme's selection is reverse video, never a color (`NO_COLOR` and `mono`).
#[test]
fn the_mono_selection_is_reversed_not_colored() {
    use radian_tui::blocks::ReasoningMode;
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    app.theme = radian_tui::theme::Theme::mono();
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.entries,
        ReasoningMode::Expanded,
        40,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    app.selection = Some(Selection {
        anchor: locate(&app.entries, ReasoningMode::Expanded, 40, 1, 8).expect("anchor"),
        head: locate(&app.entries, ReasoningMode::Expanded, 40, 1, 17).expect("head"),
    });
    let terminal = render(&mut app, 40, 6);
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
    use radian_tui::blocks::ReasoningMode;
    use radian_tui::select::{Selection, locate};
    let mut app = App::new("m", "s");
    app.push_message(&Message::user("hello selection"));
    app.set_transcript_width(40);
    let (total, _) = radian_tui::render::visible_lines(
        &mut app.entries,
        ReasoningMode::Expanded,
        40,
        0,
        0,
        &app.theme,
    );
    app.set_viewport(u16::try_from(total).unwrap(), total);
    let at = locate(&app.entries, ReasoningMode::Expanded, 40, 1, 8).expect("anchor");
    app.selection = Some(Selection {
        anchor: at,
        head: at,
    });
    let text = screen(&render(&mut app, 40, 6));
    assert!(text.contains("hello selection"), "{text}");
    assert_eq!(
        style_of(&render(&mut app, 40, 6), "selection").map(|s| s.bg),
        Some(Some(Color::Rgb(33, 59, 73))),
        "no selection band: the two anchors coincide, only the user band is there"
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
    let terminal = render(&mut app, 80, 12);
    let buffer = terminal.backend().buffer();
    // Find the long line's row: it starts with 79 'x' and ends with '…' on the same row.
    let mut found = None;
    for row in 0..12u16 {
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
    assert!(text.contains("▸ thinking (2 lines)"), "{text}");
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
    assert!(!text.contains("▸ thinking"), "{text}");

    app.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    let text = screen(&render(&mut app, 60, 10));
    assert!(text.contains("▸ thinking (2 lines)"), "{text}");
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
    let expanded = app.total_lines;
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Collapsed;
    let _ = render(&mut app, 60, 10);
    let collapsed = app.total_lines;
    assert_eq!(
        expanded,
        collapsed + 2,
        "three thinking lines become one placeholder"
    );
    assert_eq!(collapsed, 2, "the placeholder plus the answer");
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

/// The user block's band (ADR 0028 R4, owner acceptance): the `user` role's background covers
/// the whole row — the marker, the text, the trailing whitespace — and the blank rows above
/// and below it.
#[test]
fn a_user_block_paints_its_band_across_the_whole_block() {
    let mut app = App::new("m", "s");
    app.theme.user = ratatui::style::Style::default().bg(Color::Rgb(0x18, 0x18, 0x20));
    app.push_message(&Message::user("hello band"));
    let terminal = render(&mut app, 30, 8);
    let buffer = terminal.backend().buffer();
    let band = Color::Rgb(0x18, 0x18, 0x20);
    // Rows 0 (pad), 1 (text) and 2 (pad) are the block; every cell carries the band.
    for row in 0..3u16 {
        for col in 0..30u16 {
            let cell = buffer.cell((col, row)).expect("on screen");
            assert_eq!(cell.bg, band, "row {row} col {col} must carry the band");
        }
    }
    // The row below the block is not painted.
    assert_ne!(buffer.cell((0, 3)).expect("on screen").bg, band);
    assert!(screen(&terminal).contains("hello band"));
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
        app.entries.push(call);
        let terminal = render(&mut app, 40, 6);
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
    let mut result = Entry::new(EntryKind::ToolResult, "line one\nline two");
    result.is_error = true;
    app.entries.push(result);
    let terminal = render(&mut app, 40, 6);
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
    let hint = style_of(&terminal, "(↑/↓,").expect("the key hint");
    assert_eq!(hint.fg, app.theme.hint.fg, "the key hint is weak");
    // The first choice row is the selection: the emphasis (accent) role.
    let selected = style_of(&terminal, ">").expect("the selected row");
    assert_eq!(
        selected.fg, app.theme.notice_label.fg,
        "the selected row is the emphasis"
    );
}

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
        text.contains("Tab next"),
        "the keys must be on screen:\n{text}"
    );
    assert!(text.contains("Esc decline"), "{text}");
    // The box is on screen with its scrolled list.
    assert!(text.contains("question 1/1"), "{text}");
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
    ];
    for (text, width) in corpus {
        assert_eq!(
            radian_tui::render::wrap_line_count(text, width),
            radian_tui::render::wrap(text, width).len(),
            "{text:?} at {width}"
        );
    }
}
