//! The approval dialog: what it shows, how keys answer it, and what a cancelled turn does
//! with a prompt that is still up. Pure `App` state plus a `TestBackend` frame, so it runs
//! everywhere without a terminal.

use agent_core::RunStatus;
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_tui::app::{App, Effect, Input};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn request() -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "sub/../out.txt",
        "/ws/out.txt",
        json!({"path": "sub/../out.txt", "content": "hello"}),
        None,
    )
}

/// Puts a prompt up and returns the reply channel (which the **event loop** owns) and the
/// receiver its answer will arrive on.
fn prompt(
    app: &mut App,
) -> (
    tokio::sync::oneshot::Sender<ApprovalOutcome>,
    tokio::sync::oneshot::Receiver<ApprovalOutcome>,
) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    app.update(Input::Approval(request()));
    (reply, answer)
}

/// Answers the way the event loop does: the effect carries the outcome, and the loop sends it
/// through the reply channel it owns. Returns the request id the effect named.
fn answer(
    app: &mut App,
    key: KeyEvent,
    reply: tokio::sync::oneshot::Sender<ApprovalOutcome>,
) -> String {
    let result = app.on_key(key);
    assert_eq!(result.effects.len(), 1, "{:?}", result.effects);
    match &result.effects[0] {
        Effect::AnswerApproval {
            request_id,
            outcome,
        } => {
            let _ = reply.send(outcome.clone());
            request_id.clone()
        }
        other => panic!("expected an approval answer, got {other:?}"),
    }
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
        let (reply, answer_rx) = prompt(&mut app);
        assert_eq!(answer(&mut app, key(code), reply), "perm-1", "{code:?}");
        assert_eq!(answer_rx.await.unwrap(), expected, "{code:?}");
        assert!(
            app.pending_approval().is_none(),
            "the prompt is cleared after an answer"
        );
    }
}

#[tokio::test]
async fn keys_do_not_type_while_a_prompt_is_up() {
    let mut app = App::new("m", "s");
    let (_reply, _answer) = prompt(&mut app);
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.input.is_empty(),
        "keys answered the prompt: {:?}",
        app.input
    );
}

#[tokio::test]
async fn a_finished_turn_abandons_a_prompt_that_was_never_answered() {
    let mut app = App::new("m", "s");
    let (reply, answer_rx) = prompt(&mut app);
    // Cancelling the turn makes the permission layer resolve the `ask` as a denial; the UI
    // must not keep the stale prompt, and the loop must drop the reply channel.
    let result = app.update(Input::Event(agent_core::AgentEvent::RunFinished {
        status: RunStatus::Cancelled(agent_core::CancelReason::Requested),
    }));
    assert!(app.pending_approval().is_none());
    assert_eq!(
        result.effects,
        vec![Effect::AbandonApproval {
            request_id: "perm-1".into()
        }]
    );
    // The loop runs the effect: it drops the channel it owns.
    let mut reply = Some(reply);
    for effect in result.effects {
        if let Effect::AbandonApproval { .. } = effect {
            reply.take();
        }
    }
    assert!(answer_rx.await.is_err(), "the abandoned channel is dropped");
}

#[tokio::test]
async fn ctrl_c_while_a_prompt_is_up_cancels_instead_of_answering() {
    let mut app = App::new("m", "s");
    // The receiver is dropped without awaiting: Ctrl-C must not answer the prompt, so the
    // channel stays open until the turn actually ends and the prompt is cleared.
    let (_reply, _answer) = prompt(&mut app);
    app.projection.set_turn(radian_tui::TurnState::Running);
    let result = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(result.effects, vec![Effect::Cancel]);
    assert!(
        app.pending_approval().is_some(),
        "Ctrl-C cancels the turn; the prompt is cleared when the turn ends"
    );
}

#[test]
fn the_approval_box_is_rendered_over_the_input() {
    let mut app = App::new("mock", "s");
    app.update(Input::Approval(request()));
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
