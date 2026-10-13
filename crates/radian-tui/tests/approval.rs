//! The approval dialog: what it shows, how keys answer it, and what a cancelled turn does
//! with a prompt that is still up. Pure `App` state plus a `TestBackend` frame, so it runs
//! everywhere without a terminal.

use agent_core::RunStatus;
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_tui::app::ApprovalPrompt;
use radian_tui::{App, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn request() -> ApprovalRequest {
    ApprovalRequest {
        request_id: "perm-1".into(),
        call_id: "call-1".into(),
        tool: "write".into(),
        resource: "sub/../out.txt".into(),
        resolved_resource: "/ws/out.txt".into(),
        arguments: json!({"path": "sub/../out.txt", "content": "hello"}),
        reason: None,
    }
}

/// Puts a prompt up and returns the channel its answer will arrive on.
fn prompt(app: &mut App) -> tokio::sync::oneshot::Receiver<ApprovalOutcome> {
    let (reply, answer) = tokio::sync::oneshot::channel();
    app.request_approval(ApprovalPrompt {
        request: request(),
        reply,
    });
    answer
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn the_prompt_shows_the_resolved_path_not_the_raw_one() {
    // The permission layer resolves `sub/../out.txt` before asking; the prompt must show that
    // form, so a `..` cannot make the target look like it stayed under `sub/`.
    let text = render::approval_text(&request());
    assert!(text.contains("/ws/out.txt"), "{text}");
    assert!(text.contains("tool: write"), "{text}");
    assert!(text.contains("[y] allow once"), "{text}");
    assert!(text.contains("[a] always this session"), "{text}");
    assert!(text.contains("[n] deny"), "{text}");
    // The path line is the resolved one, not the raw argument.
    let path_line = text
        .lines()
        .find(|line| line.starts_with("path:"))
        .expect("a path line");
    assert_eq!(path_line, "path: /ws/out.txt");
}

#[tokio::test]
async fn keys_answer_the_prompt() {
    for (code, expected) in [
        (KeyCode::Char('y'), ApprovalOutcome::AllowOnce),
        (KeyCode::Char('a'), ApprovalOutcome::AllowForSession),
        (KeyCode::Char('n'), ApprovalOutcome::Deny { reason: None }),
        (KeyCode::Esc, ApprovalOutcome::Deny { reason: None }),
    ] {
        let mut app = App::new("m", "s");
        let answer = prompt(&mut app);
        assert!(app.on_key(key(code)).is_none(), "{code:?}");
        assert_eq!(answer.await.unwrap(), expected, "{code:?}");
        assert!(
            app.pending_approval.is_none(),
            "the prompt is cleared after an answer"
        );
    }
}

#[tokio::test]
async fn keys_do_not_type_while_a_prompt_is_up() {
    let mut app = App::new("m", "s");
    let _answer = prompt(&mut app);
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.input.is_empty(),
        "keys answered the prompt: {:?}",
        app.input
    );
}

#[tokio::test]
async fn a_finished_turn_clears_a_prompt_that_was_never_answered() {
    let mut app = App::new("m", "s");
    let answer = prompt(&mut app);
    // Cancelling the turn makes the permission layer resolve the `ask` as a denial; the UI
    // must not keep the stale prompt (and must drop the reply channel).
    app.apply_event(&agent_core::AgentEvent::RunFinished {
        status: RunStatus::Cancelled(agent_core::CancelReason::Requested),
    });
    assert!(app.pending_approval.is_none());
    assert!(answer.await.is_err(), "the abandoned channel is dropped");
}

#[tokio::test]
async fn ctrl_c_while_a_prompt_is_up_cancels_instead_of_answering() {
    let mut app = App::new("m", "s");
    // The receiver is dropped without awaiting: Ctrl-C must not answer the prompt, so the
    // channel stays open until the turn actually ends and the prompt is cleared.
    let _answer = prompt(&mut app);
    app.turn = radian_tui::TurnState::Running;
    let send = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(send, Some(radian_tui::Send::Cancel));
    assert!(
        app.pending_approval.is_some(),
        "Ctrl-C cancels the turn; the prompt is cleared when the turn ends"
    );
}

#[test]
fn the_approval_box_is_rendered_over_the_input() {
    let mut app = App::new("mock", "s");
    let (reply, _answer) = tokio::sync::oneshot::channel();
    app.request_approval(ApprovalPrompt {
        request: request(),
        reply,
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let screen = terminal.backend().to_string();
    assert!(screen.contains("approve tool call?"), "{screen}");
    assert!(screen.contains("/ws/out.txt"), "{screen}");
    assert!(screen.contains("allow once"), "{screen}");
    assert!(
        !screen.contains("type a message"),
        "the input box is replaced by the prompt: {screen}"
    );
}
