//! Ctrl-C's two-press quit: pure `App` state with an injected clock.
//!
//! The key reaches the app through the real `App::on_key_at` path, which stamps the wall
//! clock onto `Input::Cancel`; `update` itself never reads a clock.

use std::time::{Duration, Instant};

use radian_host::projection::TurnState;
use radian_host::{PendingRequest, PendingView};
use radian_permissions::ApprovalRequest;
use radian_session::{RuntimeEvent, TurnOutcome, TurnReason};
use radian_tui::app::{App, Effect, EntryKind, Input, QUIT_WINDOW};
use radian_tui::keys::Action;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

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
    app.update(Input::Pending {
        head: None,
        len: 0,
        answered_from_hub: Vec::new(),
    });
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

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// The approval popup's queue view, the same shape the loop hands the app.
fn pending_approval() -> PendingView<PendingRequest> {
    let request = ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "out.txt",
        "/ws/out.txt",
        json!({"path": "out.txt", "content": "hello"}),
        None,
    );
    PendingView {
        request_id: 1,
        origin: "session".into(),
        request: PendingRequest::Approval(request),
    }
}

fn prompt_approval(app: &mut App) {
    app.update(Input::Pending {
        head: Some(Box::new(pending_approval())),
        len: 1,
        answered_from_hub: Vec::new(),
    });
    assert!(app.pending_approval().is_some(), "the modal is open");
}

/// `/quit` is the second Ctrl-C's own action: the same quit flag and `Quit` effect. An idle
/// quit closes the inbox first, so a followup that arrives between the quit and
/// `session.stop()` is refused instead of starting a turn the stop would drop (ADR 0043:
/// abandon what waits).
#[test]
fn slash_quit_quits_an_idle_session() {
    let mut app = App::new("m", "s");
    app.input = "/quit".into();
    let result = app.on_key(key(KeyCode::Enter));
    assert!(app.quit, "`/quit` quits");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
    assert_eq!(
        result.effects,
        vec![Effect::CloseInbox, Effect::Quit],
        "the inbox closes before the quit effect"
    );
}

/// A cancelled turn's journaled boundary, as the driver reports it.
fn cancelled_turn() -> RuntimeEvent {
    RuntimeEvent::TurnFinished {
        turn_id: "t1".into(),
        outcome: TurnOutcome::Cancelled,
        reason: Some(TurnReason::Requested),
        stop_reason: None,
    }
}

/// `/quit` while a turn runs closes the inbox and cancels the turn through the session's
/// own path, then waits for the journaled boundary before leaving. Leaving at once would let
/// `session.stop()` drop the live turn, and a resume would read `TurnStarted` with no end
/// (ADR 0043's close contract: the turn is cancelled "through the same path as `cancel`",
/// and queued input is abandoned, not started).
#[test]
fn slash_quit_during_a_turn_cancels_it_before_the_quit() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    app.input = "/quit".into();
    let result = app.on_key(key(KeyCode::Enter));
    assert!(
        !app.quit,
        "the process must wait for the cancelled turn's boundary"
    );
    assert_eq!(
        result.effects,
        vec![Effect::CloseInbox, Effect::Cancel],
        "the inbox closes before the turn is cancelled: {:?}",
        result.effects
    );
    assert!(
        app.quit_after_turn.is_some(),
        "the quit waits for the boundary"
    );

    let result = app.update(Input::Runtime(cancelled_turn()));
    assert!(app.quit, "the boundary arrived: the quit runs");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
    assert!(
        matches!(result.effects.first(), Some(Effect::CloseInbox)),
        "the boundary's quit closes the inbox before it leaves: {:?}",
        result.effects
    );
}

/// A turn that never reports its boundary (a driver stuck past the cancel) still lets the
/// process leave at the deadline, the way a process stop would.
#[test]
fn the_quit_wait_ends_at_its_deadline() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    app.input = "/quit".into();
    app.on_key(key(KeyCode::Enter));
    assert!(!app.quit);
    let result = app.update(Input::QuitAfterTurnDeadline);
    assert!(app.quit, "the deadline leaves anyway");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
    assert!(app.quit_after_turn.is_none(), "the wait is over");
}

/// A pending approval modal is a layer; the command path itself still quits (the key route
/// blocks typing while the modal is up, so the submission is the one the test feeds).
#[test]
fn slash_quit_with_a_pending_approval_modal_quits() {
    let mut app = App::new("m", "s");
    prompt_approval(&mut app);
    app.input = "/quit".into();
    let result = app.update(Input::Action(Action::Send));
    assert!(app.quit, "`/quit` quits under the modal");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
}

/// An argument is a usage line like every other no-argument command, and never quits.
#[test]
fn slash_quit_with_an_argument_is_an_error_line_and_does_not_quit() {
    let mut app = App::new("m", "s");
    app.input = "/quit extra".into();
    let result = app.on_key(key(KeyCode::Enter));
    assert!(!app.quit, "a usage error must not quit");
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice == "usage: /quit (no arguments)"),
        "{:?}",
        notices(&app)
    );
}

/// A remote close during a turn closes the inbox and cancels the turn first, and journals
/// the notice only when the quit finally runs: the notice is the last durable fact before the
/// process leaves.
#[test]
fn remote_close_during_a_turn_cancels_it_before_the_quit() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    let result = app.update(Input::RemoteClose);
    assert!(
        !app.quit,
        "the close waits for the cancelled turn's boundary"
    );
    assert_eq!(
        result.effects,
        vec![Effect::CloseInbox, Effect::Cancel],
        "the inbox closes before the turn is cancelled: {:?}",
        result.effects
    );
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice == "session closed remotely"),
        "the live notice is on screen at once"
    );

    let result = app.update(Input::Runtime(cancelled_turn()));
    assert!(app.quit, "the boundary arrived: the close quits");
    assert!(
        matches!(result.effects.as_slice(), [Effect::CloseInbox, Effect::HostNotice { text }, Effect::Quit] if text == "session closed remotely"),
        "the inbox closes first and the notice journals before the quit effect: {:?}",
        result.effects
    );
}

/// The same close under a completion menu: the overlay never swallows the host's close.
#[test]
fn remote_close_during_a_turn_under_a_completion_menu_quits_after_the_cancel() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    app.update(Input::Char('/'));
    let result = app.update(Input::RemoteClose);
    assert!(!app.quit);
    assert_eq!(
        result.effects,
        vec![Effect::CloseInbox, Effect::Cancel],
        "{:?}",
        result.effects
    );
    app.update(Input::Runtime(cancelled_turn()));
    assert!(app.quit);
}

/// The control port's close hook on an idle session: one host notice in the transcript, then
/// the `/quit` path. The inbox closes first and the notice is journaled before the quit effect
/// runs (ADR 0006 §5.14), so a resume still shows it.
#[test]
fn remote_close_commits_the_notice_and_quits() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::RemoteClose);
    assert!(app.quit, "a remote close quits");
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
    assert!(
        result.effects.iter().any(|effect| matches!(
            effect,
            Effect::HostNotice { text } if text == "session closed remotely"
        )),
        "the close journals its notice first: {:?}",
        result.effects
    );
    assert_eq!(
        result.effects,
        vec![
            Effect::CloseInbox,
            Effect::HostNotice {
                text: "session closed remotely".into()
            },
            Effect::Quit,
        ],
        "the inbox closes first and the journal append runs before the quit effect: {:?}",
        result.effects
    );
    let last = app.session.entries.last().expect("the close notice");
    assert_eq!(last.kind, EntryKind::Notice);
    assert_eq!(last.text, "session closed remotely");
}

/// The closing frame: an idle remote close sets no other state, so the update itself must
/// mark the frame dirty or the loop exits before drawing the notice.
#[test]
fn remote_close_marks_the_frame_dirty() {
    let mut app = App::new("m", "s");
    let result = app.update(Input::RemoteClose);
    assert!(
        result.dirty,
        "the notice was pushed but the loop would not draw it"
    );
}

/// A remote close while the user types drops the draft the way an idle Ctrl-C quit does: the
/// text goes into the in-process history and the composer clears, so the draft file written on
/// the way out is empty.
#[test]
fn remote_close_while_typing_drops_the_draft_like_ctrl_c() {
    let mut app = App::new("m", "s");
    app.input = "a draft".into();
    app.update(Input::RemoteClose);
    assert!(app.quit);
    assert!(app.input.is_empty(), "the composer is cleared");
    assert_eq!(
        app.session.history.last().map(String::as_str),
        Some("a draft"),
        "the text is kept in the in-process history only"
    );
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice == "session closed remotely"),
        "{:?}",
        notices(&app)
    );
}

/// A remote close during a fullscreen overlay (a modal here) still quits and commits the
/// notice: the close is a host fact, not a key the overlay can swallow.
#[test]
fn remote_close_during_a_modal_quits() {
    let mut app = App::new("m", "s");
    prompt_approval(&mut app);
    let result = app.update(Input::RemoteClose);
    assert!(app.quit);
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice == "session closed remotely"),
        "{:?}",
        notices(&app)
    );
}

/// The same under a completion menu: a fullscreen overlay that owns the keys.
#[test]
fn remote_close_during_a_completion_menu_quits() {
    let mut app = App::new("m", "s");
    app.update(Input::Char('/'));
    assert!(app.slash_completion.is_some(), "the menu is open");
    let result = app.update(Input::RemoteClose);
    assert!(app.quit);
    assert!(quit_asked(&result.effects), "{:?}", result.effects);
}
