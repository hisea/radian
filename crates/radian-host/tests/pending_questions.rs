//! The question half of the pending queue (ADR 0023 U7 / ADR 0024 Q4): arrival order across
//! sessions and kinds, session-close abort, late answers.
//!
//! `tests/pending.rs` covers the approval half unchanged; this file adds the shared-queue
//! behaviour that `QueuedPrompter` needs.

use radian_coding::{
    PromptError, Prompter, Question, QuestionAnswer, QuestionKind, QuestionReply, QuestionRequest,
};
use radian_host::{
    Answered, PendingQueue, PendingRequest, PendingResponse, QueuedApprover, QueuedPrompter,
};
use radian_permissions::{ApprovalOutcome, ApprovalRequest, Approver};
use std::sync::Arc;

fn question(id: &str) -> Question {
    Question {
        header: None,
        id: id.into(),
        prompt: format!("question {id}?"),
        kind: QuestionKind::Text,
        options: Vec::new(),
        allow_other: false,
        required: false,
    }
}

fn request(id: &str) -> QuestionRequest {
    QuestionRequest::new(vec![question(id)])
}

fn approval(id: &str) -> ApprovalRequest {
    ApprovalRequest::new(
        id,
        format!("call-{id}"),
        "write",
        "out.txt",
        "/ws/out.txt",
        serde_json::json!({"path": "out.txt"}),
        None,
    )
}

fn answered(id: &str) -> QuestionAnswer {
    QuestionAnswer::new(
        "q-1",
        vec![QuestionReply {
            id: id.into(),
            text: Some("hi".into()),
            ..QuestionReply::default()
        }],
    )
}

type Queue = PendingQueue<PendingRequest, PendingResponse>;

/// Polls the queue until it holds `n` requests (a pushed request only reaches the queue when
/// its future is first polled).
async fn wait_len(queue: &Queue, n: usize) {
    for _ in 0..10_000 {
        if queue.len() == n {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the queue never reached {n} entries (saw {})", queue.len());
}

/// Two sessions ask at once: one arrival order, and answering one does not touch the other.
#[tokio::test]
async fn two_sessions_questions_keep_order_and_do_not_overwrite_each_other() {
    let queue = Arc::new(Queue::new());
    let a = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-a"));
    let b = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-b"));

    let a_task = tokio::spawn({
        let a = Arc::clone(&a);
        async move { a.ask(request("from-a")).await }
    });
    wait_len(&queue, 1).await;
    let b_task = tokio::spawn({
        let b = Arc::clone(&b);
        async move { b.ask(request("from-b")).await }
    });
    wait_len(&queue, 2).await;

    // The head is the first arrival, with its session.
    let head = queue.head().unwrap();
    assert_eq!(head.origin, "session-a");
    match &head.request {
        PendingRequest::Question(q) => assert_eq!(q.questions[0].id, "from-a"),
        other => panic!("expected a question, got {other:?}"),
    }

    // Answering the head completes only its waiter.
    queue
        .answer(
            head.request_id,
            PendingResponse::Question(Ok(answered("from-a"))),
        )
        .unwrap();
    assert_eq!(a_task.await.unwrap().unwrap().request_id, "q-1");

    // The other session's question is still the head, untouched.
    assert_eq!(queue.len(), 1);
    let head = queue.head().unwrap();
    assert_eq!(head.origin, "session-b");
    match &head.request {
        PendingRequest::Question(q) => assert_eq!(q.questions[0].id, "from-b"),
        other => panic!("expected a question, got {other:?}"),
    }
    queue
        .answer(
            head.request_id,
            PendingResponse::Question(Ok(answered("from-b"))),
        )
        .unwrap();
    assert_eq!(b_task.await.unwrap().unwrap().request_id, "q-1");
    assert!(queue.is_empty());
}

/// Closing a session aborts its own questions with `PromptError::Gone` (which the tool turns
/// into `prompter_gone`) and leaves every other session alone.
#[tokio::test]
async fn a_closed_session_aborts_its_questions_with_prompter_gone() {
    let queue = Arc::new(Queue::new());
    let a = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-a"));
    let b = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-b"));

    let a_task = tokio::spawn({
        let a = Arc::clone(&a);
        async move { a.ask(request("from-a")).await }
    });
    wait_len(&queue, 1).await;
    let b_task = tokio::spawn({
        let b = Arc::clone(&b);
        async move { b.ask(request("from-b")).await }
    });
    wait_len(&queue, 2).await;

    assert_eq!(a.abort_session(), 1);
    assert_eq!(a_task.await.unwrap().unwrap_err(), PromptError::Gone);
    assert_eq!(queue.len(), 1, "the other session is untouched");

    // The surviving question can still be answered normally.
    let head = queue.head().unwrap();
    assert_eq!(head.origin, "session-b");
    queue
        .answer(
            head.request_id,
            PendingResponse::Question(Ok(answered("from-b"))),
        )
        .unwrap();
    assert_eq!(b_task.await.unwrap().unwrap().request_id, "q-1");
}

/// An answer to a question that was already aborted is ignored, never completed twice.
#[tokio::test]
async fn a_late_answer_is_ignored() {
    let queue: Queue = Queue::new();
    let (id, answer) = queue.push("s", PendingRequest::Question(request("q")));
    assert_eq!(queue.abort_session_all("s"), 1);
    assert_eq!(
        answer.wait().await.unwrap(),
        PendingResponse::Question(Err(PromptError::Gone))
    );
    assert_eq!(
        queue.answer(id, PendingResponse::Question(Ok(answered("q")))),
        Err(Answered::Late)
    );
    assert!(queue.is_empty());
}

/// Dropping the waiting end (the turn was cancelled) removes the request and never wakes it.
#[tokio::test]
async fn a_cancelled_question_leaves_no_element() {
    let queue = Arc::new(Queue::new());
    let prompter = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-a"));
    let task = tokio::spawn({
        let prompter = Arc::clone(&prompter);
        async move { prompter.ask(request("q")).await }
    });
    wait_len(&queue, 1).await;
    task.abort();
    let _ = task.await;
    assert_eq!(queue.len(), 0, "a cancelled question must not linger");
}

/// Approvals and questions share one queue, so the UI sees a single arrival order (ADR 0023
/// U7). The `QueuedApprover` on the shared queue keeps the fail-closed denial.
#[tokio::test]
async fn approvals_and_questions_share_one_arrival_order() {
    let queue = Arc::new(Queue::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "session-a"));
    let prompter = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "session-b"));

    let approval_task = tokio::spawn({
        let approver = Arc::clone(&approver);
        async move { approver.decide(approval("r1")).await }
    });
    wait_len(&queue, 1).await;
    let question_task = tokio::spawn({
        let prompter = Arc::clone(&prompter);
        async move { prompter.ask(request("q1")).await }
    });
    wait_len(&queue, 2).await;

    // The approval arrived first and is the head; the question is behind it.
    let head = queue.head().unwrap();
    assert_eq!(head.origin, "session-a");
    assert!(matches!(head.request, PendingRequest::Approval(_)));
    queue
        .answer(
            head.request_id,
            PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        )
        .unwrap();
    assert_eq!(
        approval_task.await.unwrap().unwrap(),
        ApprovalOutcome::AllowOnce
    );

    let head = queue.head().unwrap();
    assert_eq!(head.origin, "session-b");
    assert!(matches!(head.request, PendingRequest::Question(_)));
    queue
        .answer(
            head.request_id,
            PendingResponse::Question(Ok(answered("q1"))),
        )
        .unwrap();
    assert_eq!(question_task.await.unwrap().unwrap().request_id, "q-1");
}

/// Closing a session on the shared queue aborts **both** kinds, each with its own outcome:
/// an approval is denied fail-closed (ADR 0012), a question gets `PromptError::Gone`.
#[tokio::test]
async fn a_shared_queue_session_close_aborts_both_kinds() {
    let queue = Arc::new(Queue::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "s"));
    let prompter = Arc::new(QueuedPrompter::new(Arc::clone(&queue), "s"));

    let approval_task = tokio::spawn({
        let approver = Arc::clone(&approver);
        async move { approver.decide(approval("r1")).await }
    });
    wait_len(&queue, 1).await;
    let question_task = tokio::spawn({
        let prompter = Arc::clone(&prompter);
        async move { prompter.ask(request("q1")).await }
    });
    wait_len(&queue, 2).await;

    assert_eq!(approver.abort_session(), 2);
    assert_eq!(
        approval_task.await.unwrap().unwrap(),
        ApprovalOutcome::Deny {
            reason: Some("session closed".into())
        }
    );
    assert_eq!(question_task.await.unwrap().unwrap_err(), PromptError::Gone);
    assert!(queue.is_empty());
}
