//! Rendering tests on `TestBackend`: reasoning and assistant text are distinguishable,
//! an interrupted prefix is marked, and tool calls are shown.

use agent_core::{AgentEvent, ContentPart, Message, ToolCall, ToolResult};
use radian_tui::app::{App, Entry, EntryKind};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

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
    // Live deltas arrive first...
    app.apply_event(&AgentEvent::ModelReasoningDelta {
        text: "THINKING-WORDS".into(),
    });
    app.apply_event(&AgentEvent::ModelTextDelta {
        text: "VISIBLE-ANSWER".into(),
    });
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
    assert_eq!(reasoning.fg, Some(Color::DarkGray));
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
    let marker = style_of(&terminal, "[interrupted]").unwrap();
    assert_eq!(marker.fg, Some(Color::Yellow));
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
    app.apply_event(&AgentEvent::TurnStarted);
    app.entries.push(Entry::new(EntryKind::User, "hi"));
    let terminal = render(&mut app, 60, 10);
    let text = screen(&terminal);
    assert!(text.contains("my-model"), "{text}");
    assert!(text.contains("running"), "{text}");
}

#[test]
fn live_deltas_are_replaced_by_the_committed_message_not_duplicated() {
    let mut app = App::new("m", "s");
    app.apply_event(&AgentEvent::ModelTextDelta { text: "hel".into() });
    app.apply_event(&AgentEvent::ModelTextDelta { text: "lo".into() });
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
