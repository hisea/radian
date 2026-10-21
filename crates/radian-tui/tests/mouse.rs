//! ADR 0023 U2 (plan B): the mouse wheel scrolls the transcript, not the input history.
//! Pure `App` state, so the regression (dogfood p5) has a fast test.

use radian_core::Message;
use radian_host::{PendingRequest, PendingView};
use radian_permissions::ApprovalRequest;
use radian_tui::app::{App, Input, MOUSE_WHEEL_LINES, MouseScrollDirection, Overlay};
use radian_tui::picker::Picker;
use serde_json::json;

fn request() -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "/ws/out.txt",
        "/ws/out.txt",
        json!({}),
        None,
    )
}

/// The queue's view of that request, the way the loop shows the head (ADR 0023 U7).
fn view() -> PendingView<PendingRequest> {
    PendingView {
        request_id: 1,
        origin: "session".into(),
        request: PendingRequest::Approval(request()),
    }
}

/// A transcript long enough to scroll, with the viewport pinned to the bottom.
fn scrolled_app() -> App {
    let mut app = App::new("m", "s");
    for index in 0..80 {
        app.push_message(&Message::user(format!("line {index}")));
    }
    app.set_viewport(10, 80);
    app.scroll_to_bottom();
    app
}

/// The regression: a wheel-up notch scrolls the transcript and leaves the input box alone.
/// The old alternate-scroll behavior turned it into `Up`, which replaced the draft with the
/// previous history entry.
#[test]
fn a_wheel_up_scrolls_and_keeps_the_input_box() {
    let mut app = scrolled_app();
    app.input.set_text("half-written draft");
    app.session.history.push("previous command".into());
    let before = app.session.scroll;
    let result = app.mouse_scroll(MouseScrollDirection::Up);
    assert!(result.dirty);
    assert_eq!(app.session.scroll, before - MOUSE_WHEEL_LINES);
    assert_eq!(
        app.input.text(),
        "half-written draft",
        "the wheel must not reach the input history"
    );
    assert_eq!(app.session.history_cursor, None);
}

/// A wheel-down notch scrolls back toward the bottom.
#[test]
fn a_wheel_down_scrolls_back() {
    let mut app = scrolled_app();
    app.mouse_scroll(MouseScrollDirection::Up);
    let up = app.session.scroll;
    app.mouse_scroll(MouseScrollDirection::Down);
    assert_eq!(app.session.scroll, up + MOUSE_WHEEL_LINES);
    assert!(app.is_following(), "back at the bottom pins the view again");
}

/// A modal consumes the wheel: the approval prompt does not scroll the transcript behind it.
#[test]
fn a_modal_swallows_the_wheel() {
    let mut app = scrolled_app();
    app.update(Input::Pending(Some(Box::new(view())), 1));
    let before = app.session.scroll;
    let result = app.mouse_scroll(MouseScrollDirection::Up);
    assert!(!result.dirty);
    assert_eq!(app.session.scroll, before);
}

/// An overlay scrolls itself: the picker's selection moves, and the transcript does not.
#[test]
fn a_picker_overlay_scrolls_its_own_selection() {
    let mut app = scrolled_app();
    let rows = vec![
        radian_host::sessions::SessionRow {
            session_id: "alpha".into(),
            created_ts: None,
            last_ts: None,
            message_count: 1,
            summary: "first".into(),
        },
        radian_host::sessions::SessionRow {
            session_id: "beta".into(),
            created_ts: None,
            last_ts: None,
            message_count: 1,
            summary: "second".into(),
        },
    ];
    let picker = Picker::new(rows, None, "/w");
    app.push_overlay(Overlay::Picker(picker));
    assert_eq!(
        app.picker().and_then(Picker::resume_hint).as_deref(),
        Some("--resume alpha")
    );
    let before = app.session.scroll;
    app.mouse_scroll(MouseScrollDirection::Down);
    assert_eq!(
        app.picker().and_then(Picker::resume_hint).as_deref(),
        Some("--resume beta"),
        "the wheel moved the picker's selection"
    );
    assert_eq!(
        app.session.scroll, before,
        "the transcript behind the picker did not move"
    );
}
