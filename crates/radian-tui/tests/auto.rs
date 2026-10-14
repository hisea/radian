//! ADR 0026 D9: the in-session auto-approval switch, as pure `App` state. No terminal, no
//! runtime: the key press is the real `App::on_key` path, and the effects are what the loop
//! would run. The correction (2026-10-13) pins the shape: the key is `!`, `Y`
//! stays `y` (allow once, never auto), and there is no confirmation step.

use radian_host::PendingResponse;
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_tui::app::{App, Effect, Modal, PendingApproval};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn request() -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "/ws/out.txt",
        "/ws/out.txt",
        serde_json::json!({}),
        None,
    )
}

/// The app with one approval request (id 7) on screen.
fn approval_app() -> App {
    let mut app = App::new("m", "s");
    app.push_modal(Modal::Approval(Box::new(PendingApproval {
        request_id: 7,
        origin: "session".into(),
        request: request(),
        scroll: 0,
        total_rows: 0,
        visible_rows: 0,
    })));
    app
}

fn notices(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// `!` approves the pending request once **and** turns auto on, in one press, with no
/// confirmation. The effects run in order: the answer
/// first, then the switch.
#[test]
fn the_auto_key_approves_this_request_and_turns_auto_on() {
    let mut app = approval_app();
    let effects = app.on_key(key(KeyCode::Char('!'))).effects;
    assert_eq!(
        effects,
        vec![
            Effect::AnswerRequest {
                request_id: 7,
                response: PendingResponse::Approval(ApprovalOutcome::AllowOnce),
            },
            Effect::SetApprovalPolicy { on: true },
        ],
        "{effects:?}"
    );
    assert!(app.auto, "`!` turns auto on for the session");
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("deny rules and the sandbox still apply")),
        "the notice says what auto does not lift: {:?}",
        notices(&app)
    );
}

/// The correction pins this: `Y` stays `y` — allow once — and it must **never**
/// turn auto on. (A Caps-Lock `y` is an allow once, not a mode switch.)
#[test]
fn y_stays_allow_once_and_never_enables_auto() {
    for code in [KeyCode::Char('y'), KeyCode::Char('Y')] {
        let mut app = approval_app();
        let effects = app.on_key(key(code)).effects;
        assert_eq!(
            effects,
            vec![Effect::AnswerRequest {
                request_id: 7,
                response: PendingResponse::Approval(ApprovalOutcome::AllowOnce),
            }],
            "{code:?}"
        );
        assert!(!app.auto, "{code:?} must not enable auto");
        assert!(
            !notices(&app).iter().any(|notice| notice.contains("auto:")),
            "{code:?} must not announce auto: {:?}",
            notices(&app)
        );
    }
}

/// The other answer keys keep their meanings and none of them touches auto.
#[test]
fn the_other_answer_keys_do_not_touch_auto() {
    for (code, outcome) in [
        (KeyCode::Char('a'), ApprovalOutcome::AllowForSession),
        (KeyCode::Char('A'), ApprovalOutcome::AllowForSession),
        (KeyCode::Char('n'), ApprovalOutcome::Deny { reason: None }),
        (KeyCode::Esc, ApprovalOutcome::Deny { reason: None }),
    ] {
        let mut app = approval_app();
        let effects = app.on_key(key(code)).effects;
        assert_eq!(
            effects,
            vec![Effect::AnswerRequest {
                request_id: 7,
                response: PendingResponse::Approval(outcome),
            }],
            "{code:?}"
        );
        assert!(!app.auto, "{code:?} must not enable auto");
    }
}

/// `/auto on` and `/auto off` (and `/yolo`, the alias) flip the switch and ask the loop to
/// apply it; they need no confirmation because they are typed, not one keypress.
#[test]
fn auto_on_and_off_commands() {
    for command in ["/auto on", "/yolo on"] {
        let mut app = App::new("m", "s");
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert_eq!(effects, vec![Effect::SetApprovalPolicy { on: true }], "{command}");
        assert!(app.auto, "{command} turns auto on");
    }
    for command in ["/auto off", "/yolo off"] {
        let mut app = App::new("m", "s");
        app.set_auto(true);
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert_eq!(effects, vec![Effect::SetApprovalPolicy { on: false }], "{command}");
        assert!(!app.auto, "{command} turns auto off");
    }
}

/// A wrong `/auto` argument is a usage notice, never a silent state change.
#[test]
fn a_wrong_auto_argument_is_a_usage_notice() {
    for command in ["/auto", "/auto maybe", "/auto ON", "/yolo"] {
        let mut app = App::new("m", "s");
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert!(effects.is_empty(), "{command}: {effects:?}");
        assert!(!app.auto, "{command} must not enable auto");
        assert!(
            notices(&app).iter().any(|notice| notice.contains("usage: /auto on|off")),
            "{command}: {:?}",
            notices(&app)
        );
    }
}

/// `/auto` and `/yolo` are on the command tree, so completion offers them and their one
/// argument level offers `on`/`off` (the brief: both hang on the command tree).
#[test]
fn auto_and_yolo_complete_from_the_command_tree() {
    use radian_tui::command;
    let tree = command::tree();
    for name in ["auto", "yolo"] {
        let entry = tree
            .iter()
            .find(|command| command.name == name)
            .unwrap_or_else(|| panic!("{name} is on the tree"));
        let level = (entry.levels[0])(&[], "");
        let command::Level::Ready(candidates) = level else {
            panic!("{name}'s first level is ready, got {level:?}");
        };
        let words: Vec<&str> = candidates
            .iter()
            .map(|candidate| candidate.insert.as_str())
            .collect();
        assert_eq!(words, vec!["on", "off"], "{name}");
    }
}
