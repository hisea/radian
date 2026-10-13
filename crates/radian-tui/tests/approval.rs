//! The approval dialog: what it shows, how keys answer it, and how the pending queue drives
//! it (ADR 0023 U7). Pure `App` state plus a `TestBackend` frame, so it runs everywhere
//! without a terminal.

use agent_core::{AgentEvent, CancelReason, RunStatus};
use radian_host::{PendingRequest, PendingResponse, PendingView};
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

/// The queue's view of that request: the id the UI shows and answers (ADR 0023 U7).
fn view() -> PendingView<PendingRequest> {
    PendingView {
        request_id: 1,
        origin: "session".into(),
        request: PendingRequest::Approval(request()),
    }
}

/// Puts the queue's head on screen the way the loop does.
fn prompt(app: &mut App) {
    app.update(Input::Pending(Some(Box::new(view())), 1));
}

/// Answers the way the event loop does: the effect names the queue's request id and the
/// response, and the loop hands both to the facade. Returns what the effect named.
fn answer(app: &mut App, key: KeyEvent) -> (u64, ApprovalOutcome) {
    let result = app.on_key(key);
    assert_eq!(result.effects.len(), 1, "{:?}", result.effects);
    match result.effects[0].clone() {
        Effect::AnswerRequest {
            request_id,
            response,
        } => match response {
            PendingResponse::Approval(outcome) => (request_id, outcome),
            other => panic!("expected an approval response, got {other:?}"),
        },
        other => panic!("expected an answer request, got {other:?}"),
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

#[test]
fn keys_answer_the_prompt_with_the_queue_id() {
    for (code, expected) in [
        (KeyCode::Char('y'), ApprovalOutcome::AllowOnce),
        (KeyCode::Char('a'), ApprovalOutcome::AllowForSession),
        (KeyCode::Char('n'), ApprovalOutcome::Deny { reason: None }),
        (KeyCode::Esc, ApprovalOutcome::Deny { reason: None }),
    ] {
        let mut app = App::new("m", "s");
        prompt(&mut app);
        let (request_id, outcome) = answer(&mut app, key(code));
        assert_eq!(request_id, 1, "{code:?}");
        assert_eq!(outcome, expected, "{code:?}");
        assert!(
            app.pending_approval().is_none(),
            "the prompt is cleared after an answer"
        );
    }
}

#[test]
fn keys_do_not_type_while_a_prompt_is_up() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.input.is_empty(),
        "keys answered the prompt: {:?}",
        app.input
    );
}

/// ADR 0023 U7: the queue drives the modal, not the turn. A finished turn produces no effect
/// for the prompt; the head going away (the permission layer's waiter was cancelled) is what
/// removes it, so "the waiter gave up" and "the turn ended" are the same event.
#[test]
fn the_queue_head_drives_the_prompt_not_the_turn() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    let result = app.update(Input::Event(AgentEvent::RunFinished {
        status: RunStatus::Cancelled(CancelReason::Requested),
    }));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(
        app.pending_approval().is_some(),
        "the turn ending does not by itself clear the modal"
    );

    app.update(Input::Pending(None, 0));
    assert!(app.pending_approval().is_none(), "the queue's head is gone");
    assert_eq!(app.pending, 0);
}

#[test]
fn ctrl_c_while_a_prompt_is_up_cancels_instead_of_answering() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    app.projection.set_turn(radian_tui::TurnState::Running);
    let result = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(result.effects, vec![Effect::Cancel]);
    assert!(
        app.pending_approval().is_some(),
        "Ctrl-C cancels the turn; the queue clears the prompt when the waiter goes"
    );
}

/// Two approvals in a row: the second does not supersede the first (ADR 0023 U7 replaced that
/// behavior). The queue hands the UI one head at a time.
#[test]
fn a_second_approval_does_not_supersede_the_first() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    let second = PendingView {
        request_id: 2,
        origin: "session".into(),
        request: PendingRequest::Approval(request()),
    };
    // The queue's head is still the first request (the second is behind it), so nothing moves.
    app.update(Input::Pending(Some(Box::new(view())), 2));
    assert_eq!(
        app.pending_approval().map(|r| r.request_id.as_str()),
        Some("perm-1")
    );
    assert_eq!(app.pending, 2, "the status bar counts the queue");

    // When the first is answered, the second becomes the head.
    app.update(Input::Pending(Some(Box::new(second)), 1));
    assert!(app.pending_approval().is_some());
    assert_eq!(app.pending, 1);
}

#[test]
fn the_approval_box_is_rendered_over_the_input() {
    let mut app = App::new("mock", "s");
    prompt(&mut app);
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
