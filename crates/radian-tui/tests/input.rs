//! Input handling: Enter sends (followup when idle, steer while a turn runs), Ctrl-J and
//! Shift-Enter insert a newline, Ctrl-C cancels then quits.
//!
//! These are pure `update` tests (ADR 0023 U3): a key becomes an `Effect`, and the effect's
//! result comes back as an `Input`. Nothing here starts a turn.

use radian_tui::app::{App, Effect, Input, TurnState};
use radian_tui::mention::{ExpandError, Expansion, Rejection};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The effects of one key event. The loop calls `App::update` directly.
fn press(app: &mut App, key: KeyEvent) -> Vec<Effect> {
    app.on_key(key).effects
}

/// What `mention::expand` returns for a message with no `@` reference.
fn expanded(text: &str) -> Expansion {
    Expansion {
        text: text.to_owned(),
        attachments: Vec::new(),
        warnings: Vec::new(),
    }
}

#[test]
fn enter_expands_then_sends_a_followup_when_idle_and_a_steer_while_running() {
    let mut app = App::new("m", "s");
    app.input = "hello".into();
    // Sending is two steps now: the loop expands the `@` references, then the result is the
    // send itself. The input is taken (and cleared) at the first step, exactly as before.
    assert_eq!(
        press(&mut app, key(KeyCode::Enter)),
        vec![Effect::ExpandMentions {
            text: "hello".into()
        }]
    );
    assert!(app.input.is_empty(), "the input is cleared on send");
    let result = app.update(Input::MentionsExpanded(Ok(expanded("hello"))));
    assert_eq!(
        result.effects,
        vec![Effect::SendFollowup(agent_core::Message::user("hello"))]
    );

    app.session.projection.set_turn(TurnState::Running);
    app.input = "change of plan".into();
    assert_eq!(
        press(&mut app, key(KeyCode::Enter)),
        vec![Effect::ExpandMentions {
            text: "change of plan".into()
        }]
    );
    let result = app.update(Input::MentionsExpanded(Ok(expanded("change of plan"))));
    assert_eq!(
        result.effects,
        vec![Effect::Steer(agent_core::Message::user("change of plan"))]
    );
}

/// A failed `@` expansion must not lose the message the user typed: nothing is sent and the
/// text is back in the input box.
#[test]
fn a_failed_expansion_puts_the_input_back_and_sends_nothing() {
    let mut app = App::new("m", "s");
    app.input = "look @missing.txt".into();
    assert_eq!(
        press(&mut app, key(KeyCode::Enter)),
        vec![Effect::ExpandMentions {
            text: "look @missing.txt".into()
        }]
    );
    let result = app.update(Input::MentionsExpanded(Err(ExpandError::Unusable(vec![
        Rejection {
            path: "@missing.txt".into(),
            reason: "no such file".into(),
        },
    ]))));
    assert!(result.effects.is_empty(), "nothing is sent");
    assert_eq!(app.input, "look @missing.txt", "the text is back");
    assert!(
        app.session
            .entries
            .iter()
            .any(|entry| entry.text.contains("not sent")),
        "the failure is shown"
    );
}

#[test]
fn an_empty_input_is_not_sent() {
    let mut app = App::new("m", "s");
    assert!(press(&mut app, key(KeyCode::Enter)).is_empty());
    app.input = "   ".into();
    assert!(press(&mut app, key(KeyCode::Enter)).is_empty());
}

#[test]
fn ctrl_j_inserts_a_newline_and_enter_sends_the_whole_text() {
    let mut app = App::new("m", "s");
    app.on_key(key(KeyCode::Char('a')));
    app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    app.on_key(key(KeyCode::Char('b')));
    assert_eq!(app.input, "a\nb");
    assert_eq!(
        press(&mut app, key(KeyCode::Enter)),
        vec![Effect::ExpandMentions {
            text: "a\nb".into()
        }]
    );
    let result = app.update(Input::MentionsExpanded(Ok(expanded("a\nb"))));
    assert_eq!(
        result.effects,
        vec![Effect::SendFollowup(agent_core::Message::user("a\nb"))]
    );
}

#[test]
fn shift_enter_inserts_a_newline() {
    // The terminal only reports Enter+SHIFT when the keyboard-enhancement flags are on (the
    // kitty protocol; see `terminal.rs`); this is the app-level half: a shifted Enter is a
    // newline and **never** a send.
    let mut app = App::new("m", "s");
    let result = app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert!(result.effects.is_empty(), "Shift+Enter must not send");
    app.on_key(key(KeyCode::Char('x')));
    assert_eq!(app.input, "\nx");
    // A plain Enter still sends, and Ctrl-J still inserts a newline (the fallback on a
    // terminal that cannot report Shift+Enter).
    let plain = press(&mut app, key(KeyCode::Enter));
    assert!(
        matches!(plain.as_slice(), [Effect::ExpandMentions { .. }]),
        "{plain:?}"
    );
    let mut app = App::new("m", "s");
    let ctrl_j = app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert!(ctrl_j.effects.is_empty(), "Ctrl-J must not send");
    assert_eq!(app.input, "\n");
}

#[test]
fn ctrl_c_cancels_a_running_turn_and_quits_when_idle_after_a_second_press() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    assert_eq!(
        press(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        vec![Effect::Cancel]
    );
    assert!(!app.quit);

    // Idle and empty, the first press only arms the 2 s quit window;
    // the second inside it quits.
    app.session.projection.set_turn(TurnState::Idle);
    assert!(
        press(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        )
        .is_empty()
    );
    assert!(!app.quit, "the first press only arms");
    assert_eq!(
        press(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        vec![Effect::Quit]
    );
    assert!(app.quit);
}
