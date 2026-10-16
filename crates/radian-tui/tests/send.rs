//! ADR 0030 slice 7b: the TUI's `/send` — the parse, the loop effect, and the two notices (the
//! "sent to" line and the bounded reply).
//!
//! Pure `App` state on a `TestBackend`: the inputs the loop synthesizes are fed exactly as the
//! loop feeds them, and the key press is the real `App::on_key` path.

use radian_tui::app::{App, Effect, Input};
use radian_tui::command::{Outcome, SendRequest};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
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

/// `/send` with no argument lists; a name needs text; anything else is a usage notice.
#[test]
fn send_parses_list_and_a_name_with_text() {
    assert!(matches!(
        radian_tui::command::parse("/send", true),
        Outcome::Send(SendRequest::List)
    ));
    match radian_tui::command::parse("/send writer fix the typo", true) {
        Outcome::Send(SendRequest::Send { name, text }) => {
            assert_eq!(name, "writer");
            assert_eq!(text, "fix the typo");
        }
        other => panic!("expected a send, got {other:?}"),
    }
    match radian_tui::command::parse("/send writer", true) {
        Outcome::Notices(notices) => assert!(notices[0].contains("usage: /send"), "{notices:?}"),
        other => panic!("expected a usage notice, got {other:?}"),
    }
}

/// A submitted `/send …` line is the host effect the loop runs, with the `User` source.
#[test]
fn a_submitted_send_line_is_the_loop_effect() {
    let mut app = App::new("mock", "s");
    app.input = "/send writer fix it".into();
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::Send(SendRequest::Send {
            name: "writer".to_owned(),
            text: "fix it".to_owned(),
        })],
        "{effects:?}"
    );
    // The human typed it: the loop enqueues `/send` as `User`, never `Subagent` (ADR 0030 O12).
    assert_eq!(
        radian_tui::command::send_source(),
        agent_core::InputSource::User
    );
}

/// The "sent to" and "replied" notices reach the transcript (and the frame) as notice lines.
#[test]
fn the_send_notices_are_shown() {
    let mut app = App::new("mock", "s");

    let dirty = app.update(Input::AgentSent {
        name: "writer".into(),
        created: true,
    });
    assert!(dirty.dirty, "a sent notice must redraw");
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("created `writer`")),
        "{:?}",
        notices(&app)
    );

    let dirty = app.update(Input::AgentReplied {
        name: "writer".into(),
        summary: "fixed the typo\nand the test".into(),
    });
    assert!(dirty.dirty, "a reply notice must redraw");
    let text = notices(&app).join("\n");
    assert!(text.contains("`writer` replied"), "{text}");
    assert!(text.contains("fixed the typo"), "{text}");
    // The bounded reply is visible on the rendered frame (TestBackend).
    let frame = screen(&mut app, 120, 12);
    assert!(frame.contains("writer"), "{frame}");
}

/// `/send` is on the command tree and `/help` lists it.
#[test]
fn send_is_on_the_tree_and_in_help() {
    assert!(
        radian_tui::command::tree()
            .iter()
            .any(|command| command.name == "send"),
        "send is on the tree"
    );
    assert!(
        radian_tui::command::HELP.contains("/send"),
        "/help lists /send"
    );
}
