//! The pending-request queue (ADR 0023 U7): order, source, answer, abandonment, session
//! abort, fail-closed approval and the multi-threaded interleaving.

use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use radian_host::{Answered, PendingAnswer, PendingQueue, QueuedApprover};
use radian_permissions::{ApprovalOutcome, ApprovalRequest, Approver};

fn request(id: &str) -> ApprovalRequest {
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

/// The next version after `changed()` resolves, with a watchdog so a missing notification
/// fails instead of hanging.
async fn changed(rx: &mut tokio::sync::watch::Receiver<u64>) {
    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .expect("the queue must notify on a change")
        .expect("the watch sender is alive");
}

#[tokio::test]
async fn two_sessions_keep_arrival_order_and_do_not_overwrite_each_other() {
    let queue: PendingQueue<u32, u32> = PendingQueue::new();
    let (first_id, first) = queue.push("session-a", 10);
    let (second_id, second) = queue.push("session-b", 20);
    let (third_id, third) = queue.push("session-a", 30);
    assert!(first_id < second_id && second_id < third_id);

    // The head is the first arrival, with its origin.
    let head = queue.head().unwrap();
    assert_eq!(head.request_id, first_id);
    assert_eq!(head.origin, "session-a");
    assert_eq!(head.request, 10);
    assert_eq!(queue.len(), 3);

    // Answering the head leaves the others in order.
    queue.answer(first_id, 11).unwrap();
    assert_eq!(first.await.unwrap(), 11);
    let head = queue.head().unwrap();
    assert_eq!(head.request_id, second_id);
    assert_eq!(head.request, 20);
    assert_eq!(queue.len(), 2);

    // Answering the second does not touch the first session's third request.
    queue.answer(second_id, 21).unwrap();
    assert_eq!(second.await.unwrap(), 21);
    assert_eq!(queue.head().unwrap().request_id, third_id);
    assert_eq!(queue.len(), 1);
    queue.answer(third_id, 31).unwrap();
    assert_eq!(third.await.unwrap(), 31);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn answer_completes_the_waiter_and_notifies() {
    let queue: PendingQueue<u32, u32> = PendingQueue::new();
    let mut rx = queue.subscribe();
    let (id, answer) = queue.push("s", 7);
    changed(&mut rx).await; // push notified
    queue.answer(id, 70).unwrap();
    changed(&mut rx).await; // answer notified
    assert_eq!(answer.await.unwrap(), 70);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_dropped_waiter_disappears_and_a_late_answer_is_late() {
    let queue: PendingQueue<u32, u32> = PendingQueue::new();
    let (dropped_id, dropped) = queue.push("s", 1);
    let (kept_id, kept) = queue.push("s", 2);
    let mut rx = queue.subscribe();
    drop(dropped); // the turn was cancelled: nobody is waiting for `dropped_id` any more

    // The drop itself wakes the UI (the waiting end bumps the version).
    changed(&mut rx).await;
    assert_eq!(queue.len(), 1, "the abandoned request is gone");
    assert_eq!(queue.head().unwrap().request_id, kept_id);
    assert_eq!(queue.answer(dropped_id, 9), Err(Answered::Late));
    assert!(queue.answer(kept_id, 20).is_ok());
    assert_eq!(kept.await.unwrap(), 20);
    assert_eq!(queue.answer(999, 0), Err(Answered::Unknown));
}

#[tokio::test]
async fn abort_session_completes_only_that_session() {
    let queue: PendingQueue<u32, u32> = PendingQueue::new();
    let (a1, answer_a1) = queue.push("session-a", 1);
    let (b1, answer_b1) = queue.push("session-b", 2);
    let (_a2, answer_a2) = queue.push("session-a", 3);
    let aborted = queue.abort_session("session-a", 0);
    assert_eq!(aborted, 2);
    assert_eq!(answer_a1.await.unwrap(), 0);
    assert_eq!(answer_a2.await.unwrap(), 0);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue.head().unwrap().request_id, b1);
    // The other session still waits and can be answered normally.
    queue.answer(b1, 22).unwrap();
    assert_eq!(answer_b1.await.unwrap(), 22);
    assert!(queue.is_empty());
    // A late answer to the aborted id is late, not a duplicate.
    assert_eq!(queue.answer(a1, 1), Err(Answered::Late));
}

#[tokio::test]
async fn dropping_the_queue_abandons_every_waiter() {
    let queue: PendingQueue<u32, u32> = PendingQueue::new();
    let (_id1, answer1) = queue.push("s", 1);
    let (_id2, answer2) = queue.push("s", 2);
    drop(queue);
    let (one, two) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(2), answer1.wait()),
        tokio::time::timeout(Duration::from_secs(2), answer2.wait()),
    );
    assert!(one.expect("must not hang").is_err());
    assert!(two.expect("must not hang").is_err());
}

#[tokio::test]
async fn the_queued_approver_is_fail_closed() {
    let queue = Arc::new(PendingQueue::<ApprovalRequest, ApprovalOutcome>::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "session-a"));

    // A pushed approval is denied when the session closes, with the ADR 0012 fail-closed
    // outcome.
    let a = Arc::clone(&approver);
    let task = tokio::spawn(async move { a.decide(request("r1")).await });
    loop {
        if queue.len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(approver.abort_session(), 1);
    let outcome = task.await.unwrap().unwrap();
    assert_eq!(
        outcome,
        ApprovalOutcome::Deny {
            reason: Some("session closed".into())
        }
    );
    assert!(queue.is_empty());
}

#[tokio::test]
async fn a_cancelled_approval_leaves_no_element() {
    let queue = Arc::new(PendingQueue::<ApprovalRequest, ApprovalOutcome>::new());
    let approver = Arc::new(QueuedApprover::new(Arc::clone(&queue), "session-a"));

    // The permission layer polls `decide`, then the turn is cancelled and the future is
    // dropped. The waiting end goes with it.
    let a = Arc::clone(&approver);
    let task = tokio::spawn(async move { a.decide(request("r2")).await });
    loop {
        if queue.len() == 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    // `len` cleans up the closed receiver.
    assert_eq!(queue.len(), 0, "a cancelled approval must not linger");
}

#[test]
fn a_thousand_interleaved_pushes_and_answers() {
    let queue = Arc::new(PendingQueue::<u32, u32>::new());
    type Answers = Arc<Mutex<Vec<(u64, PendingAnswer<u32>)>>>;
    let answers: Answers = Arc::new(Mutex::new(Vec::new()));
    let barrier = Arc::new(Barrier::new(2));

    let q = Arc::clone(&queue);
    let a = Arc::clone(&answers);
    let b = Arc::clone(&barrier);
    let producer = std::thread::spawn(move || {
        for i in 0..1000u32 {
            let (id, answer) = q.push("s", i);
            a.lock().unwrap().push((id, answer));
            b.wait();
        }
    });

    let q = Arc::clone(&queue);
    let a = Arc::clone(&answers);
    let b = Arc::clone(&barrier);
    let consumer = std::thread::spawn(move || {
        let mut results = Vec::with_capacity(1000);
        for i in 0..1000usize {
            b.wait();
            // Hold the whole `answers` vector alive; only index it.
            let (id, _answer) = &a.lock().unwrap()[i];
            let id = *id;
            results.push(q.answer(id, id as u32));
        }
        results
    });

    producer.join().unwrap();
    let results = consumer.join().unwrap();
    assert_eq!(results.len(), 1000);
    assert!(results.iter().all(|r| *r == Ok(())), "{results:?}");
    assert!(queue.is_empty());
}
