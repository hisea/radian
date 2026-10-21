//! Public queue semantics of the four input operations (ADR 0011): concurrency, the
//! bounded-refusal path, `cancel`'s exemptions, and `inject` not waking the driver.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::{InputSource, Message, MessageId, MessageMeta, OpaqueJson};
use common::{TempDir, created};
use radian_runtime::{CancelCause, Inbox, InboxError, Lane};
use radian_session::{EventData, Session, SessionJournal};

// ---------- 1. concurrent followup: nothing lost, FIFO by enqueue order ----------

#[tokio::test]
async fn concurrent_followups_are_all_kept_and_ordered_by_enqueue() {
    let inbox = Inbox::new(256);
    let mut tasks = Vec::new();
    for i in 0..64u32 {
        let inbox = Arc::clone(&inbox);
        tasks.push(tokio::spawn(async move {
            inbox
                .followup(format!("m{i}"), InputSource::User)
                .await
                .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.unwrap());
    }

    let snapshot = inbox.snapshot().await;
    assert_eq!(snapshot.len(), 64, "no message lost, none duplicated");
    assert_eq!(inbox.len().await, 64);
    let mut seen: Vec<MessageId> = snapshot.iter().map(|e| e.id.clone()).collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    seen.dedup();
    assert_eq!(seen.len(), 64, "ids are unique");

    // The queue is FIFO by enqueue order: the monotonic `seq` runs 0..n without gaps.
    let seqs: Vec<u64> = snapshot.iter().map(|e| e.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "queue order is enqueue order");
    assert_eq!(seqs, (0..64).collect::<Vec<_>>());
    assert!(snapshot.iter().all(|e| e.lane == Lane::NextTurn));
}

// ---------- 2. full queue: refuse, hand the message back, keep the queue intact ----------

#[tokio::test]
async fn a_full_inbox_refuses_and_returns_the_rejected_message() {
    let inbox = Inbox::new(2);
    inbox.followup("a", InputSource::User).await.unwrap();
    inbox.steer("b", InputSource::User).await.unwrap();
    assert_eq!(inbox.len().await, 2);

    for (op, text) in [("followup", "c"), ("steer", "d"), ("inject", "e")] {
        let error = match op {
            "followup" => inbox.followup(text, InputSource::User).await.unwrap_err(),
            "steer" => inbox.steer(text, InputSource::User).await.unwrap_err(),
            "inject" => inbox.inject(text, InputSource::User).await.unwrap_err(),
            other => panic!("unknown op {other}"),
        };
        match &error {
            InboxError::Full { message, capacity } => {
                assert_eq!(*capacity, 2);
                assert_eq!(
                    message.text(),
                    text,
                    "the error carries the rejected message"
                );
            }
            other => panic!("expected Full, got {other:?}"),
        }
        assert_eq!(error.message().text(), text);
        assert_eq!(error.into_message().text(), text);
    }

    // Nothing was evicted and nothing was added.
    let snapshot = inbox.snapshot().await;
    assert_eq!(
        snapshot
            .iter()
            .map(|e| e.message.text())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(inbox.len().await, 2);
}

#[tokio::test]
async fn only_user_messages_are_accepted() {
    let inbox = Inbox::new(4);
    let assistant = Message::assistant("not a user message");
    let error = inbox
        .followup(assistant.clone(), InputSource::User)
        .await
        .unwrap_err();
    assert!(matches!(error, InboxError::NotUser { .. }));
    assert_eq!(error.into_message(), assistant);
    assert!(inbox.is_empty().await);
}

// ---------- 3. cancel: no lane, no capacity, idempotent ----------

#[tokio::test]
async fn cancel_ignores_capacity_and_is_idempotent() {
    let inbox = Inbox::new(1);
    inbox.followup("full", InputSource::User).await.unwrap();
    assert_eq!(inbox.len().await, 1);

    // Not subject to capacity: it succeeds with a full queue.
    inbox.cancel(CancelCause::Requested);
    assert_eq!(inbox.cancel_generation(), 1);
    assert_eq!(inbox.last_cancel_cause(), Some(CancelCause::Requested));

    // Repeated cancel: no error, no second effect.
    inbox.cancel(CancelCause::Requested);
    inbox.cancel(CancelCause::DeadlineExceeded);
    assert_eq!(
        inbox.cancel_generation(),
        1,
        "only one cancellation episode"
    );
    assert_eq!(
        inbox.last_cancel_cause(),
        Some(CancelCause::Requested),
        "a repeated cancel does not overwrite the recorded cause"
    );
    assert_eq!(inbox.cancel_calls(), 3);

    // cancel did not occupy the queue.
    assert_eq!(inbox.len().await, 1);
    assert!(matches!(
        inbox
            .followup("another", InputSource::User)
            .await
            .unwrap_err(),
        InboxError::Full { .. }
    ));

    // A new turn starts a new episode.
    inbox.begin_turn();
    inbox.cancel(CancelCause::Requested);
    assert_eq!(inbox.cancel_generation(), 2);
}

// ---------- 5. inject does not wake ----------

#[tokio::test(start_paused = true)]
async fn inject_does_not_wake_an_idle_driver_but_followup_and_steer_do() {
    let inbox = Inbox::new(8);

    // `inject` must not resolve the wake signal.
    inbox.inject("context", InputSource::System).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_err(),
        "inject must not wake the driver"
    );

    // `steer` does.
    inbox
        .steer("change direction", InputSource::User)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_ok(),
        "steer wakes the driver"
    );

    // `followup` does.
    inbox
        .followup("next task", InputSource::User)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_ok(),
        "followup wakes the driver"
    );

    // `cancel` does too.
    inbox.cancel(CancelCause::Requested);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_ok(),
        "cancel wakes the driver"
    );
}

#[tokio::test(start_paused = true)]
async fn closing_rejects_new_input_and_wakes_the_driver() {
    let inbox = Inbox::new(4);
    inbox.close();
    assert!(inbox.is_closed());
    match inbox.followup("late", InputSource::User).await {
        Err(InboxError::Closed { message }) => assert_eq!(message.text(), "late"),
        other => panic!("expected Closed, got {other:?}"),
    }
    // Closing wakes an idle driver so it can stop.
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_ok()
    );
}

// ---------- lane mapping and source passthrough ----------

#[tokio::test]
async fn the_four_operations_map_to_the_documented_lanes() {
    let inbox = Inbox::new(8);
    inbox.followup("f", InputSource::User).await.unwrap();
    inbox.steer("s", InputSource::User).await.unwrap();
    inbox.inject("i", InputSource::User).await.unwrap();

    let snapshot = inbox.snapshot().await;
    let by_text: Vec<(String, Lane)> = snapshot
        .iter()
        .map(|e| (e.message.text(), e.lane))
        .collect();
    assert!(by_text.contains(&("f".to_owned(), Lane::NextTurn)));
    assert!(by_text.contains(&("s".to_owned(), Lane::NextStep)));
    assert!(by_text.contains(&("i".to_owned(), Lane::NextStep)));

    // Claim order within a lane is FIFO.
    let first = inbox.claim_next_step().await.unwrap();
    assert_eq!(first.message.text(), "s");
    let second = inbox.claim_next_step().await.unwrap();
    assert_eq!(second.message.text(), "i");
    assert!(inbox.claim_next_step().await.is_none());
    assert_eq!(inbox.claim_next_turn().await.unwrap().message.text(), "f");
}

/// The source is required at the runtime boundary  and is
/// recorded on the message itself so the transcript and the durable event agree.
#[tokio::test]
async fn source_is_required_and_recorded_on_the_message() {
    let inbox = Inbox::new(8);

    inbox
        .followup("no source", InputSource::Scheduler)
        .await
        .unwrap();
    let entry = inbox.snapshot().await.pop().unwrap();
    assert_eq!(entry.source, InputSource::Scheduler);
    match &entry.message {
        Message::User { meta, .. } => {
            assert_eq!(meta.source, Some(InputSource::Scheduler));
            assert!(meta.id.is_some(), "the inbox assigns an id");
        }
        other => panic!("expected a user message, got {other:?}"),
    }

    // A caller-supplied opaque origin survives; the source is overwritten with the declared
    // one (the API is the source of truth, not the pre-built message).
    let meta = MessageMeta {
        id: None,
        source: Some(InputSource::User),
        origin: Some(OpaqueJson::parse(r#"{"parent":{"session_id":"s"}}"#).unwrap()),
    };
    let message = Message::User {
        content: vec![agent_core::ContentPart::text("from a subagent")],
        meta,
    };
    inbox.inject(message, InputSource::Subagent).await.unwrap();
    let entry = inbox
        .snapshot()
        .await
        .into_iter()
        .find(|e| e.message.text() == "from a subagent")
        .unwrap();
    assert_eq!(entry.source, InputSource::Subagent);
    match &entry.message {
        Message::User { meta, .. } => {
            assert_eq!(meta.source, Some(InputSource::Subagent));
            assert!(meta.origin.is_some());
        }
        other => panic!("expected a user message, got {other:?}"),
    }
}

/// A degraded inject waits on the `next_step` lane like `inject` (never waking an idle
/// driver), and its durable `InboxEnqueued` record says so (`degraded: true`, ADR 0006
/// §5.5); a plain `inject` records `false`.
#[tokio::test]
async fn a_degraded_inject_marks_its_durable_record() {
    let dir = TempDir::new("degraded");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(8, Arc::clone(&journal));

    inbox.inject("plain", InputSource::System).await.unwrap();
    inbox
        .inject_degraded("degraded", InputSource::System)
        .await
        .unwrap();

    // Both wait on the no-wake lane.
    let lanes: Vec<Lane> = inbox
        .snapshot()
        .await
        .iter()
        .map(|entry| entry.lane)
        .collect();
    assert_eq!(lanes, vec![Lane::NextStep, Lane::NextStep]);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_err(),
        "neither inject wakes the driver"
    );

    let session = Session::open(&path).unwrap();
    let degraded: Vec<(String, bool)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::InboxEnqueued {
                message_id,
                degraded,
                ..
            } => Some((message_id.0.clone(), *degraded)),
            _ => None,
        })
        .collect();
    assert_eq!(degraded.len(), 2);
    assert!(!degraded[0].1, "a plain inject is not a degrade");
    assert!(degraded[1].1, "a degraded inject is marked");
}

/// A turn-opening claim is already visible in the in-flight count when the claim
/// returns: the pop and the count share the queue lock, so a host checking between
/// the pop and the turn's start still sees the claimed turn (exec -p's exit check).
/// Mutation: move the count out of `claim_next_turn` (let the caller set it
/// afterwards) → the first `inflight_turns` assertion fails.
#[tokio::test]
async fn a_claimed_turn_is_inflight_before_the_claim_returns() {
    let inbox = Inbox::new(8);
    assert_eq!(inbox.inflight_turns(), 0, "nothing claimed yet");
    inbox.followup("turn", InputSource::User).await.unwrap();
    assert_eq!(inbox.inflight_turns(), 0, "queued, not yet claimed");
    let entry = inbox
        .claim_next_turn()
        .await
        .expect("the followup opens a turn");
    assert_eq!(entry.lane, Lane::NextTurn);
    assert_eq!(
        inbox.inflight_turns(),
        1,
        "claimed: visible before any driver step runs"
    );
    // An empty claim changes nothing.
    assert!(inbox.claim_next_turn().await.is_none());
    assert_eq!(inbox.inflight_turns(), 1);
    // The turn ends: back to zero.
    inbox.turn_ended();
    assert_eq!(inbox.inflight_turns(), 0);

    // A waking `next_step` entry claimed through the turn lane counts the same way;
    // a plain `inject` never opens a turn and never counts.
    inbox.steer("steer", InputSource::User).await.unwrap();
    inbox.inject("plain", InputSource::User).await.unwrap();
    inbox
        .claim_next_turn()
        .await
        .expect("the waking steer opens a turn");
    assert_eq!(inbox.inflight_turns(), 1);
    inbox.turn_ended();
    assert!(inbox.claim_next_turn().await.is_none());
    assert_eq!(
        inbox.inflight_turns(),
        0,
        "a plain inject is not a turn-opening claim"
    );
}

// ---------- 6. pending wake (ADR 0011) ----------

/// `claim_next_turn` order: `next_turn` first, then a waking `next_step` entry, then the
/// oldest pending-wake `next_step` entry. A plain `inject` never opens a turn.
/// Mutation: `claim_next_turn` ignores the pending flag → the third claim fails.
#[tokio::test]
async fn claim_next_turn_prefers_next_turn_then_wake_then_pending_wake() {
    let inbox = Inbox::new(8);
    // Enqueued oldest-first: the claim order must still be lane/flag priority, not FIFO.
    inbox.inject("plain", InputSource::User).await.unwrap();
    inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();
    inbox.steer("steer", InputSource::User).await.unwrap();
    inbox.followup("task", InputSource::User).await.unwrap();

    let first = inbox.claim_next_turn().await.unwrap();
    assert_eq!(first.message.text(), "task");
    assert!(!first.pending_wake);
    inbox.turn_ended();
    let second = inbox.claim_next_turn().await.unwrap();
    assert_eq!(second.message.text(), "steer");
    assert!(!second.pending_wake);
    inbox.turn_ended();
    let third = inbox.claim_next_turn().await.unwrap();
    assert_eq!(third.message.text(), "notice");
    assert!(third.pending_wake, "the stranded notice carries its flag");
    inbox.turn_ended();
    assert!(
        inbox.claim_next_turn().await.is_none(),
        "a plain inject never opens a turn, even with nothing else queued"
    );
}

/// The oldest pending-wake entry wins among pending-wake entries.
#[tokio::test]
async fn the_oldest_pending_wake_entry_opens_first() {
    let inbox = Inbox::new(8);
    inbox
        .inject_pending_wake("first", InputSource::System)
        .await
        .unwrap();
    inbox
        .inject_pending_wake("second", InputSource::System)
        .await
        .unwrap();
    assert_eq!(
        inbox.claim_next_turn().await.unwrap().message.text(),
        "first"
    );
    inbox.turn_ended();
    assert_eq!(
        inbox.claim_next_turn().await.unwrap().message.text(),
        "second",
        "the next claim takes the next pending-wake entry"
    );
    inbox.turn_ended();
    assert!(inbox.claim_next_turn().await.is_none());
}

/// The promotion signal fires exactly once per pending-wake entry claimed to open a
/// turn: not for a plain `inject`, not for a `steer`, and not when the entry is absorbed
/// mid-turn through `claim_next_step`.
#[tokio::test]
async fn the_pending_wake_callback_fires_only_for_a_promoted_open() {
    use std::sync::Mutex;
    let inbox = Inbox::new(8);
    let fired: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fired_in = Arc::clone(&fired);
    inbox.set_pending_wake_callback(Some(Arc::new(move |id: MessageId| {
        fired_in.lock().unwrap().push(id.0.clone());
    })));

    let notice = inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();
    inbox.inject("plain", InputSource::User).await.unwrap();
    inbox.steer("steer", InputSource::User).await.unwrap();
    // The steer opens first (flag priority); the callback stays silent for it.
    inbox.claim_next_turn().await.unwrap();
    inbox.turn_ended();
    assert!(fired.lock().unwrap().is_empty());
    // The pending-wake notice opens next: exactly one firing, with its id.
    inbox.claim_next_turn().await.unwrap();
    inbox.turn_ended();
    assert_eq!(*fired.lock().unwrap(), vec![notice.0.clone()]);
    // A pending-wake entry absorbed mid-turn never fires. (`plain` is still queued
    // ahead of it, so the step lane is drained first.)
    let absorbed = inbox
        .inject_pending_wake("absorbed", InputSource::System)
        .await
        .unwrap();
    assert_eq!(
        inbox.claim_next_step().await.unwrap().message.text(),
        "plain"
    );
    assert_eq!(inbox.claim_next_step().await.unwrap().id, absorbed);
    assert!(inbox.claim_next_turn().await.is_none());
    assert_eq!(
        *fired.lock().unwrap(),
        vec![notice.0.clone()],
        "absorbing {absorbed:?} mid-turn fires nothing"
    );
}

/// A pending-wake enqueue notifies the driver (like a steer), so an enqueue that lands
/// after the driver's last claim still wakes the loop to look; a plain `inject` stays
/// silent.
#[tokio::test(start_paused = true)]
async fn a_pending_wake_enqueue_notifies_while_a_plain_inject_stays_silent() {
    let inbox = Inbox::new(8);
    inbox.inject("context", InputSource::System).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_err(),
        "a plain inject must not wake the driver"
    );
    inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), inbox.wake_signal())
            .await
            .is_ok(),
        "a pending-wake notice wakes the driver to look"
    );
}

/// A pending-wake inject records its durable mark (`pending_wake: true`); a plain
/// `inject` records `false` (an ordinary line stays byte-identical to before).
#[tokio::test]
async fn a_pending_wake_inject_marks_its_durable_record() {
    let dir = TempDir::new("pending-wake");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(8, Arc::clone(&journal));

    inbox.inject("plain", InputSource::System).await.unwrap();
    inbox
        .inject_pending_wake("notice", InputSource::System)
        .await
        .unwrap();

    // Both wait on the no-wake lane.
    let lanes: Vec<Lane> = inbox
        .snapshot()
        .await
        .iter()
        .map(|entry| entry.lane)
        .collect();
    assert_eq!(lanes, vec![Lane::NextStep, Lane::NextStep]);

    let session = Session::open(&path).unwrap();
    let marked: Vec<(String, bool)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::InboxEnqueued {
                message_id,
                pending_wake,
                ..
            } => Some((message_id.0.clone(), *pending_wake)),
            _ => None,
        })
        .collect();
    assert_eq!(marked.len(), 2);
    assert!(!marked[0].1, "a plain inject carries no pending wake");
    assert!(marked[1].1, "a pending-wake notice is marked");
}
