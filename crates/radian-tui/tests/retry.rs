//! `/retry`: resend the last user message after a model-error turn.
//!
//! Pure `update` tests (ADR 0023 U3): only an idle session whose last turn failed
//! (`TurnFailed` seen, no new turn started since) resends — as a direct `SendFollowup` with
//! the transcript's folded text, never re-expanded. A normal end, a cancel, a running turn
//! or no turn at all is a "nothing to retry" notice.

use agent_core::Message;
use radian_session::{RuntimeEvent, TurnOutcome};
use radian_tui::app::{App, Effect, Entry, EntryKind, Input, TurnState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Submits `/retry` like the loop (no `@` expansion is involved on this path).
fn submit_retry(app: &mut App) -> Vec<Effect> {
    app.input = "/retry".into();
    app.on_key(key(KeyCode::Enter)).effects
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn failed(app: &mut App, message: &str) {
    app.update(Input::Runtime(RuntimeEvent::TurnFailed {
        turn_id: "t-1".into(),
        message: message.into(),
    }));
}

fn finished(app: &mut App, outcome: TurnOutcome) {
    app.update(Input::Runtime(RuntimeEvent::TurnFinished {
        turn_id: "t-1".into(),
        outcome,
        reason: None,
        stop_reason: None,
    }));
}

fn user_entry(app: &mut App, text: &str) {
    app.session.entries.push(Entry::new(EntryKind::User, text));
}

/// After a model-error turn `/retry` resends the last user message as one followup — with no
/// `@` expansion step — and the failure notice names `/retry`.
#[test]
fn retry_after_a_failed_turn_resends_the_last_user_message() {
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    failed(&mut app, "boom");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("turn failed: boom") && note.contains("/retry to resend")),
        "{:?}",
        notices(&app)
    );

    let effects = submit_retry(&mut app);
    assert_eq!(
        effects,
        vec![Effect::SendFollowup(Message::user("do the thing"))],
        "a direct followup, never an expansion or a steer"
    );
}

/// A normal end, a cancel, a running turn and no turn at all are all "nothing to retry".
/// In particular a cancelled turn must not arm `/retry` — allowing it there is the
/// regression this test pins (only `TurnFailed` arms it).
#[test]
fn retry_is_refused_without_a_failed_turn() {
    // No turn at all.
    let mut app = App::new("m", "s");
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "{:?}",
        notices(&app)
    );

    // A completed turn.
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    finished(&mut app, TurnOutcome::Completed);
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "{:?}",
        notices(&app)
    );

    // A cancelled turn: no failure, so nothing to retry.
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    finished(&mut app, TurnOutcome::Cancelled);
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "{:?}",
        notices(&app)
    );

    // A failure, then a new turn that is cancelled: the new start cleared the flag.
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    failed(&mut app, "boom");
    app.update(Input::Runtime(RuntimeEvent::TurnStarted {
        turn_id: "t-2".into(),
    }));
    finished(&mut app, TurnOutcome::Cancelled);
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "{:?}",
        notices(&app)
    );

    // While a turn runs `/retry` is refused even with the flag armed.
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    failed(&mut app, "boom");
    app.session.projection.set_turn(TurnState::Running);
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "{:?}",
        notices(&app)
    );
}

/// The retry sends once: sending clears the flag, and the next turn's start clears it too.
#[test]
fn retry_arms_only_until_it_is_used_or_a_new_turn_starts() {
    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    failed(&mut app, "boom");
    assert_eq!(submit_retry(&mut app).len(), 1);
    assert!(submit_retry(&mut app).is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("nothing to retry")),
        "a second /retry is refused: {:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    user_entry(&mut app, "do the thing");
    failed(&mut app, "boom");
    app.update(Input::Runtime(RuntimeEvent::TurnStarted {
        turn_id: "t-2".into(),
    }));
    assert!(submit_retry(&mut app).is_empty());
}

/// An attachment is resent as the transcript's folded text — verbatim, with no re-read of
/// the file and no `@` expansion.
#[test]
fn retry_resends_the_folded_text_verbatim() {
    let mut app = App::new("m", "s");
    user_entry(&mut app, "read this [file: notes.txt, 12 lines]");
    failed(&mut app, "boom");
    assert_eq!(
        submit_retry(&mut app),
        vec![Effect::SendFollowup(Message::user(
            "read this [file: notes.txt, 12 lines]"
        ))],
        "the folded text goes as-is"
    );
}
