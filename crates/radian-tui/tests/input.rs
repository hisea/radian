//! Input handling: Enter sends (followup when idle, steer while a turn runs), Ctrl-J and
//! Shift-Enter insert a newline, Ctrl-C cancels then quits.

use radian_tui::app::{App, Send, TurnState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn enter_sends_a_followup_when_idle_and_a_steer_while_running() {
    let mut app = App::new("m", "s");
    app.input = "hello".into();
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Send::Followup("hello".into()))
    );
    assert!(app.input.is_empty(), "the input is cleared on send");

    app.turn = TurnState::Running;
    app.input = "change of plan".into();
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Send::Steer("change of plan".into()))
    );
}

#[test]
fn an_empty_input_is_not_sent() {
    let mut app = App::new("m", "s");
    assert_eq!(app.on_key(key(KeyCode::Enter)), None);
    app.input = "   ".into();
    assert_eq!(app.on_key(key(KeyCode::Enter)), None);
}

#[test]
fn ctrl_j_inserts_a_newline_and_enter_sends_the_whole_text() {
    let mut app = App::new("m", "s");
    app.on_key(key(KeyCode::Char('a')));
    app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('b')));
    assert_eq!(app.input, "a\nb");
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        Some(Send::Followup("a\nb".into()))
    );
}

#[test]
fn shift_enter_inserts_a_newline() {
    let mut app = App::new("m", "s");
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    app.on_key(key(KeyCode::Char('x')));
    assert_eq!(app.input, "\nx");
}

#[test]
fn ctrl_c_cancels_a_running_turn_and_quits_when_idle() {
    let mut app = App::new("m", "s");
    app.turn = TurnState::Running;
    assert_eq!(
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Send::Cancel)
    );
    assert!(!app.quit);

    app.turn = TurnState::Idle;
    assert_eq!(
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(Send::Quit)
    );
    assert!(app.quit);
}
