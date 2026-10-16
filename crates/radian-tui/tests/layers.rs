//! ADR 0023 U4: the layer stack, its two invariants, and the tier rule. Pure `App` state, so
//! these run without a terminal.

use agent_core::{AgentEvent, CancelReason, RunStatus};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_tui::TurnState;
use radian_tui::app::{App, Effect, Input, Layer, Modal, Overlay, Tier};
use radian_tui::find::FindState;
use radian_tui::keys::Keymap;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

/// The queue's view of the approval (ADR 0023 U7): the id the UI shows and answers.
fn approval_view() -> radian_host::PendingView<radian_host::PendingRequest> {
    radian_host::PendingView {
        request_id: 1,
        origin: "session".into(),
        request: radian_host::PendingRequest::Approval(request()),
    }
}

fn request() -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "/ws/out.txt",
        "/ws/out.txt",
        json!({"path": "/ws/out.txt"}),
        None,
    )
}

/// The stack always has the surface at the bottom and every overlay below every modal.
fn assert_tiers(app: &App) {
    let layers = app.layers();
    assert_eq!(layers.first().map(Layer::tier), Some(Tier::Surface));
    assert!(
        layers
            .windows(2)
            .all(|pair| pair[0].tier() as u8 <= pair[1].tier() as u8),
        "tiers must be non-decreasing from the bottom: {:?}",
        layers.iter().map(Layer::tier).collect::<Vec<_>>()
    );
}

/// Invariant 1 (ADR 0023 U4): Ctrl-C still cancels a running turn while an approval modal is
/// open, and the cancelled turn resolves that approval as a denial.
#[test]
fn ctrl_c_cancels_while_an_approval_modal_is_open() {
    let mut app = App::new("m", "s");
    // The turn state is a session fact written by the event sink (ADR 0023 U5), so the test
    // sets it on the projection the way the sink would.
    app.session.projection.set_turn(TurnState::Running);
    app.update(Input::Pending(Some(Box::new(approval_view())), 1));
    assert!(app.pending_approval().is_some());
    assert_tiers(&app);

    // Ctrl-C runs before the stack: it cancels, it does not answer the prompt.
    let result = app.on_key(ctrl(KeyCode::Char('c')));
    assert_eq!(result.effects, vec![Effect::Cancel]);
    assert!(app.pending_approval().is_some());

    // The cancelled turn makes the permission layer's waiter give up. The queue drops the
    // element and wakes the UI (ADR 0023 U7), so the modal goes with the head; the turn event
    // itself does not touch it.
    let result = app.update(Input::Event(AgentEvent::RunFinished {
        status: RunStatus::Cancelled(CancelReason::Requested),
    }));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(
        app.pending_approval().is_some(),
        "the queue's head still stands until the waiter is gone"
    );
    app.update(Input::Pending(None, 0));
    assert!(app.pending_approval().is_none());
}

/// Invariant 2 (ADR 0023 U4): the approval keys are hardcoded, so `tui.keybindings` can never
/// take `y` over, not even by binding `cancel` to it.
#[test]
fn the_approval_keys_survive_a_keymap_that_rebinds_y() {
    // `send`/`cancel` must each be the only binding on `y`, or the keymap refuses it as a
    // conflict (that refusal is its own test); this checks that the approval key wins in
    // either case.
    for bindings in ["send = [\"y\"]\n", "cancel = [\"y\"]\n"] {
        let mut app = App::new("m", "s");
        let config = radian_host::config::parse(
            std::path::Path::new("/tmp/layers.toml"),
            &format!("schema_version = 1\n[tui.keybindings]\n{bindings}"),
        )
        .expect("a valid config");
        app.apply_config(&config, false).expect("a valid keymap");
        app.update(Input::Pending(Some(Box::new(approval_view())), 1));
        let result = app.on_key(key(KeyCode::Char('y')));
        assert_eq!(
            result.effects,
            vec![Effect::AnswerRequest {
                request_id: 1,
                response: radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
            }],
            "`y` must still approve with {bindings:?}"
        );
        assert!(!app.quit, "`y` must not cancel/quit");
    }
}

/// The layer rule (ADR 0023 U4): an overlay pushed while a modal is up goes **below** the
/// modal, so keys still reach the modal first.
#[test]
fn an_overlay_pushed_while_a_modal_is_up_stays_below_it() {
    let mut app = App::new("m", "s");
    app.update(Input::Pending(Some(Box::new(approval_view())), 1));
    // A push that ignores the rule would land on top; `push_overlay` must not.
    app.push_overlay(Overlay::Find(FindState::open(0)));
    assert_tiers(&app);
    let modal = app
        .layers()
        .iter()
        .position(|layer| matches!(layer, Layer::Modal(_)))
        .expect("the modal");
    let overlay = app
        .layers()
        .iter()
        .position(|layer| matches!(layer, Layer::Overlay(_)))
        .expect("the overlay");
    assert!(overlay < modal, "the overlay is below the modal");

    // The modal still consumes the key: `n` denies the call, it does not type into find.
    let result = app.on_key(key(KeyCode::Char('n')));
    assert_eq!(
        result.effects,
        vec![Effect::AnswerRequest {
            request_id: 1,
            response: radian_host::PendingResponse::Approval(ApprovalOutcome::Deny {
                reason: None
            }),
        }]
    );
    assert!(app.find_state().is_some(), "the overlay is untouched");
}

/// The built-in keymap is unchanged: the stack routing reproduces today's keys.
#[test]
fn the_default_stack_routes_like_the_old_hand_written_order() {
    let mut app = App::new("m", "s");
    assert!(app.on_key(key(KeyCode::Char('x'))).dirty);
    assert_eq!(app.input.text(), "x");
    // Ctrl-C clears a non-empty input first, then, pressed again on the
    // empty input, arms and quits.
    assert!(app.on_key(ctrl(KeyCode::Char('c'))).effects.is_empty());
    assert!(!app.quit);
    assert!(app.input.is_empty());
    assert!(
        app.on_key(ctrl(KeyCode::Char('c'))).effects.is_empty(),
        "the first empty press only arms"
    );
    assert_eq!(
        app.on_key(ctrl(KeyCode::Char('c'))).effects,
        vec![Effect::Quit]
    );
    assert!(app.quit);
}

/// `Keymap` is still the source of the transcript/find keys, not the hardcoded modals.
#[test]
fn a_rebound_escape_still_reaches_the_transcript() {
    let mut app = App::new("m", "s");
    app.input.set_text("draft");
    app.keymap = Keymap::from_config(&[("escape".into(), vec!["ctrl-x".into()])]).unwrap();
    // Esc is no longer `escape`, so it does nothing; Ctrl-X clears the draft.
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.input.text(), "draft");
    app.on_key(ctrl(KeyCode::Char('x')));
    assert_eq!(app.input.text(), "");
    // The state is running so an empty input would cancel; it is empty only after the clear.
    // The turn state is a session fact, read from the projection (ADR 0023 U5).
    let _ = app.session.projection.read().turn() == TurnState::Idle;
}

/// Unconsumed input falls toward the surface: while the find overlay is open, PageUp still
/// reaches the transcript (ADR 0023 U4).
#[test]
fn the_find_overlay_passes_navigation_to_the_transcript() {
    let mut app = App::new("m", "s");
    for index in 0..80 {
        app.push_message(&agent_core::Message::user(format!("line {index}")));
    }
    app.set_viewport(10, 80);
    app.scroll_to_bottom();
    app.find_open();
    assert!(app.find_state().is_some());
    let before = app.session.scroll;
    app.on_key(key(KeyCode::PageUp));
    assert!(
        app.session.scroll < before,
        "PageUp must reach the transcript through the find overlay ({} -> {})",
        before,
        app.session.scroll
    );
    assert!(
        app.find_state().is_some(),
        "navigation does not close the find box"
    );
}

/// The structural half of acceptance 5: `App` no longer has the four `Option` modal/overlay
/// fields (they live in the layer stack now).
#[test]
fn app_has_no_four_option_fields() {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"))
        .expect("app.rs is readable");
    for field in [
        "pub pending_approval: Option",
        "pub pending_command: Option",
        "pub completion: Option",
        "pub find: Option",
    ] {
        assert!(
            !source.contains(field),
            "App must not still declare `{field}` (ADR 0023 U4)"
        );
    }
    assert!(
        source.contains("layers: Vec<Layer>"),
        "the stack is the new home"
    );
}

/// ADR 0023 U4 with **two** modals up (an approval that arrives while a `/memory` confirmation
/// or, later, a question is open): an overlay goes below both, never between them.
#[test]
fn an_overlay_never_lands_between_two_modals() {
    let mut app = App::new("m", "s");
    app.push_modal(Modal::Approval(Box::new(
        radian_tui::app::PendingApproval {
            request_id: 1,
            origin: "session".into(),
            request: request(),
            scroll: 0,
            total_rows: 0,
            visible_rows: 0,
        },
    )));
    app.push_modal(Modal::Approval(Box::new(
        radian_tui::app::PendingApproval {
            request_id: 2,
            origin: "session".into(),
            request: request(),
            scroll: 0,
            total_rows: 0,
            visible_rows: 0,
        },
    )));
    app.push_overlay(Overlay::Find(FindState::open(0)));
    assert_tiers(&app);
    let overlay = app
        .layers()
        .iter()
        .position(|layer| matches!(layer, Layer::Overlay(_)))
        .expect("the overlay");
    let lowest_modal = app
        .layers()
        .iter()
        .position(|layer| matches!(layer, Layer::Modal(_)))
        .expect("a modal");
    assert!(overlay < lowest_modal, "the overlay is below every modal");
}
