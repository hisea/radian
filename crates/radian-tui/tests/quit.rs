//! Ctrl-C's two-press quit: pure `App` state with an injected clock.
//!
//! The key reaches the app through the real `App::on_key_at` path, which stamps the wall
//! clock onto `Input::Cancel`; `update` itself never reads a clock.

use std::time::{Duration, Instant};

use radian_host::projection::TurnState;
use radian_tui::app::{App, Effect, Input, QUIT_WINDOW};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn quit_asked(effects: &[Effect]) -> bool {
    effects.iter().any(|effect| matches!(effect, Effect::Quit))
}

/// Idle with text, the first Ctrl-C clears the input into the history (like `Esc`) and never
/// quits; the quit window stays unarmed.
#[test]
fn idle_with_text_the_first_ctrl_c_clears_it_into_the_history() {
    let mut app = App::new("m", "s");
    app.input = "a draft".into();
    let result = app.on_key_at(ctrl_c(), Instant::now());
    assert!(!app.quit, "clearing must not quit");
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert!(app.input.is_empty(), "the input is cleared");
    assert_eq!(
        app.session.history.last().map(String::as_str),
        Some("a draft")
    );
    assert!(app.quit_pending.is_none(), "the window is not armed");
}

/// Idle and empty, the first Ctrl-C only arms the window: no quit, and the state the status
/// bar reads is set (dirty) so the hint appears.
#[test]
fn idle_and_empty_the_first_ctrl_c_only_arms_the_window() {
    let mut app = App::new("m", "s");
    let now = Instant::now();
    let result = app.on_key_at(ctrl_c(), now);
    assert!(!app.quit);
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert!(result.dirty, "the hint must be drawn");
    assert_eq!(app.quit_pending, Some(now + QUIT_WINDOW));
}

/// A second Ctrl-C inside the window quits.
#[test]
fn a_second_ctrl_c_inside_the_window_quits() {
    let mut app = App::new("m", "s");
    let now = Instant::now();
    app.on_key_at(ctrl_c(), now);
    let result = app.on_key_at(ctrl_c(), now + Duration::from_millis(1000));
    assert!(app.quit, "the second press quits");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
}

/// A second Ctrl-C after the window closed does not quit: it re-arms the window.
#[test]
fn a_second_ctrl_c_after_the_window_re_arms() {
    let mut app = App::new("m", "s");
    let now = Instant::now();
    app.on_key_at(ctrl_c(), now);
    let later = now + QUIT_WINDOW + Duration::from_millis(1);
    let result = app.on_key_at(ctrl_c(), later);
    assert!(!app.quit, "an expired window must not fire");
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert_eq!(app.quit_pending, Some(later + QUIT_WINDOW));
}

/// Any other key disarms the window; a later Ctrl-C then arms it afresh instead of quitting.
#[test]
fn another_key_disarms_the_window() {
    let mut app = App::new("m", "s");
    let now = Instant::now();
    app.on_key_at(ctrl_c(), now);
    // A key that does not edit the input (an editor key would fill it, and then Ctrl-C
    // would clear the input instead of arming — which is also correct).
    app.on_key_at(key(KeyCode::Up), now + Duration::from_millis(200));
    assert!(app.quit_pending.is_none(), "another key disarms");
    let result = app.on_key_at(ctrl_c(), now + Duration::from_millis(300));
    assert!(!app.quit, "the arming press must not quit after a disarm");
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert!(app.quit_pending.is_some(), "it arms again");
}

/// The loop's deadline input disarms and redraws, so the hint disappears with no key.
#[test]
fn the_deadline_input_disarms_and_redraws() {
    let mut app = App::new("m", "s");
    app.on_key_at(ctrl_c(), Instant::now());
    let result = app.update(Input::QuitDeadline);
    assert!(app.quit_pending.is_none());
    assert!(result.dirty, "the hint must be erased");
    assert!(!app.quit);
}

/// A running turn still cancels at once, and cancelling does not arm the quit window.
#[test]
fn ctrl_c_while_running_cancels() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    let result = app.on_key_at(ctrl_c(), Instant::now());
    assert!(matches!(result.effects.as_slice(), [Effect::Cancel]));
    assert!(!app.quit, "a cancel must not quit");
    assert!(app.quit_pending.is_none());
}

/// `Esc` never quits: idle and empty it does nothing at all — no quit, no
/// effect, no repaint, and it does not arm the Ctrl-C window. Quitting stays the two-press
/// Ctrl-C (above).
#[test]
fn esc_never_quits_when_idle_and_empty() {
    let mut app = App::new("m", "s");
    let result = app.on_key_at(key(KeyCode::Esc), Instant::now());
    assert!(!app.quit, "Esc must not quit an idle session");
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert!(!result.dirty, "Esc does nothing, so nothing repaints");
    assert!(
        app.quit_pending.is_none(),
        "Esc does not arm the Ctrl-C quit window"
    );
}

/// Background events and asynchronous results must not disarm the quit window: a resize, a queue change, a clipboard confirmation or a completion answer is
/// not the user changing their mind. Before the fix any of these made the second Ctrl-C only
/// re-arm, which read as "pressing twice sometimes does not quit".
#[test]
fn background_events_do_not_disarm_the_quit_window() {
    let mut app = App::new("m", "s");
    let t0 = Instant::now();
    app.on_key_at(ctrl_c(), t0);
    assert!(app.quit_pending.is_some(), "the first press arms");

    // Host facts and asynchronous results, in no particular order.
    app.update(Input::Resize {
        rows: 30,
        cols: 100,
    });
    app.update(Input::Pending(None, 0));
    app.update(Input::ClipboardDone("copied 3 chars".into()));
    app.update(Input::CompletionReady(None));
    assert!(
        app.quit_pending.is_some(),
        "a background event must not disarm"
    );

    let result = app.on_key_at(ctrl_c(), t0 + Duration::from_millis(1000));
    assert!(app.quit, "the second press still quits");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
}

/// The other direction: real user input (a typed character here) still disarms, so a press
/// after it only re-arms.
#[test]
fn user_input_still_disarms_the_quit_window() {
    let mut app = App::new("m", "s");
    let t0 = Instant::now();
    app.on_key_at(ctrl_c(), t0);
    // Clear the character out of the input so the later Ctrl-C arms instead of clearing it.
    app.update(Input::Char('x'));
    app.input = String::new().into();
    assert!(app.quit_pending.is_none(), "a typed character disarms");

    let result = app.on_key_at(ctrl_c(), t0 + Duration::from_millis(2000));
    assert!(!app.quit, "the press after a disarm only re-arms");
    assert!(!quit_asked(&result.effects), "{:?}", result.effects);
    assert!(app.quit_pending.is_some());
}
