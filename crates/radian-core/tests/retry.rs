//! ADR 0017: a transient model failure is retried inside one model call, before
//! anything is committed, and the failed attempt's streamed text never reaches the transcript.
//!
//! The retry waits use the paused test clock (`start_paused`), so a 2 s/4 s/8 s backoff costs
//! no wall time and the assertions are on the announced delays, not on elapsed seconds.

mod common;

use std::sync::Arc;
use std::time::Duration;

use radian_core::*;
use common::*;

fn agent_with_retries(
    model: Arc<ScriptedModel>,
    retries: u32,
) -> Arc<Agent<Arc<ScriptedModel>, NoTools>> {
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        retries,
        ..Default::default()
    }))
}

/// Every `ModelRetrying` the turn announced, as `(attempt, max, delay)`.
fn announced(rec: &Recorder) -> Vec<(u32, u32, Duration)> {
    rec.events()
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::ModelRetrying {
                attempt,
                max,
                delay,
                ..
            } => Some((attempt, max, delay)),
            _ => None,
        })
        .collect()
}

fn delays(rec: &Recorder) -> Vec<Duration> {
    announced(rec).into_iter().map(|(_, _, d)| d).collect()
}

/// The headline: two dropped streams, then success. The turn succeeds, the retries are one
/// model call, and only the successful answer is committed.
#[tokio::test(start_paused = true)]
async fn two_dropped_streams_are_retried_and_the_turn_succeeds() {
    let cut = "the stream ended before the response was complete";
    let model = ScriptedModel::new([
        Reply::StreamFail(vec!["half".into(), "way".into()], cut.into()),
        Reply::StreamFail(vec!["lost".into()], cut.into()),
        text("done"),
    ]);
    let mut chat = agent_with_retries(Arc::clone(&model), 3)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let outcome = run.run(&rec).await.unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(
        outcome.model_calls, 1,
        "the retries are still one model call"
    );
    let assistants: Vec<String> = run
        .transcript()
        .iter()
        .filter(|m| matches!(m, Message::Assistant { .. }))
        .map(Message::text)
        .collect();
    assert_eq!(assistants, ["done"], "only the successful answer is kept");
    assert_eq!(model.requests().len(), 3, "one request per attempt");
    let announced = announced(&rec);
    assert_eq!(announced.len(), 2, "two retries were announced");
    assert_eq!((announced[0].0, announced[0].1), (1, 3));
    assert_eq!((announced[1].0, announced[1].1), (2, 3));
    // The failed attempts' deltas did reach the host live (a host clears the block on
    // `ModelRetrying`); they just never entered the transcript.
    assert!(
        rec.events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ModelTextDelta { text } if text == "half"))
    );
}

/// `Retry-After` from the provider replaces the backoff and is honoured exactly.
#[tokio::test(start_paused = true)]
async fn a_retry_after_is_honoured_exactly() {
    let model = ScriptedModel::new([
        Reply::Unavailable("HTTP 429: slow down".into(), Some(Duration::from_secs(1))),
        text("ok"),
    ]);
    let mut chat = agent_with_retries(model, 3).open_session().await.unwrap();
    let rec = Recorder::default();

    let outcome = chat
        .run_turn("go", TurnOptions::default(), &rec)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(announced(&rec), [(1, 3, Duration::from_secs(1))]);
}

/// The waits of one model call add up to at most 60 s, even when every `Retry-After` asks for
/// more: 40 s, then 20 s, and then the budget is spent, so the turn stops (no 0 s retry).
#[tokio::test(start_paused = true)]
async fn the_total_wait_is_capped_at_sixty_seconds() {
    let busy = || Reply::Unavailable("HTTP 503: busy".into(), Some(Duration::from_secs(40)));
    let model = ScriptedModel::new([busy(), busy(), busy()]);
    let mut chat = agent_with_retries(Arc::clone(&model), 3)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    assert!(matches!(
        err,
        RunError::Model(ModelError::Unavailable { .. })
    ));
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(
        delays(&rec),
        [Duration::from_secs(40), Duration::from_secs(20)],
        "40 + 20 fills the 60 s budget; the third would be a 0 s wait, so it never happens"
    );
    assert!(delays(&rec).iter().sum::<Duration>() <= Duration::from_secs(60));
    assert_eq!(model.requests().len(), 3, "one try plus two retries");
}

/// Once the 60 s wait budget is spent the core must stop retrying, not fire the remaining
/// attempts back to back: the provider just said it is busy, so hammering it is worse than
/// failing. `Retry-After: 60` leaves room for exactly one retry.
#[tokio::test(start_paused = true)]
async fn a_spent_wait_budget_stops_retrying_instead_of_firing_back_to_back() {
    let busy = || Reply::Unavailable("HTTP 503: busy".into(), Some(Duration::from_secs(60)));
    let model = ScriptedModel::new([busy(), busy(), busy(), busy()]);
    let mut chat = agent_with_retries(Arc::clone(&model), 3)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    let RunError::Model(ModelError::Unavailable { message, .. }) = &err else {
        panic!("expected an unavailable error: {err}");
    };
    assert!(message.contains("(after 1 retry)"), "{message}");
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(
        delays(&rec),
        [Duration::from_secs(60)],
        "exactly one retry; the 60 s budget is then spent"
    );
    assert_eq!(model.requests().len(), 2, "one try plus one retry");
}

/// Without a `Retry-After` the backoff is 2 s, 4 s, 8 s (plus a little jitter), and the
/// exhausted turn fails with the "after N retries" note.
#[tokio::test(start_paused = true)]
async fn exhausted_retries_fail_with_a_note_and_double_the_backoff() {
    let fail = || Reply::Fail("network down".into());
    let model = ScriptedModel::new([fail(), fail(), fail()]);
    let mut chat = agent_with_retries(Arc::clone(&model), 2)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    let RunError::Model(ModelError::Transport(message)) = &err else {
        panic!("expected a transport error: {err}");
    };
    assert!(message.contains("(after 2 retries)"), "{message}");
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(model.requests().len(), 3, "one try plus two retries");
    let delays = delays(&rec);
    assert_eq!(delays.len(), 2);
    assert!(
        delays[0] >= Duration::from_secs(2) && delays[0] <= Duration::from_millis(2_500),
        "the first backoff is about 2 s: {:?}",
        delays[0]
    );
    assert!(
        delays[1] >= Duration::from_secs(4) && delays[1] <= Duration::from_secs(5),
        "the second backoff is about 4 s: {:?}",
        delays[1]
    );
}

/// Cancelling while the core waits to retry ends the turn `cancelled` and sends no further
/// request. The cancellation fires from the event sink, synchronously, when the retry is
/// announced — no clock race.
#[tokio::test(start_paused = true)]
async fn cancelling_during_the_backoff_ends_the_turn_without_another_request() {
    let model = ScriptedModel::new([
        Reply::Unavailable("HTTP 503: busy".into(), None),
        text("never sent"),
    ]);
    let mut chat = agent_with_retries(Arc::clone(&model), 3)
        .open_session()
        .await
        .unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let handle = run.cancellation_handle();
    let events = move |event: AgentEvent| {
        if matches!(event, AgentEvent::ModelRetrying { .. }) {
            handle.cancel();
        }
    };

    // The paused clock: if the cancellation interrupts the wait, no virtual time passes.
    let started = tokio::time::Instant::now();
    let outcome = run.run(&events).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert_eq!(
        model.requests().len(),
        1,
        "no request after the cancellation"
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::ZERO,
        "the cancellation must end the wait at once, not after the backoff"
    );
}

/// `retries = 0` is the pre-revision behaviour: the first transient failure fails the turn,
/// with no retry event and no "after N retries" note.
#[tokio::test]
async fn retries_zero_fails_on_the_first_transient_error() {
    let model = ScriptedModel::new([Reply::Fail("network down".into()), text("unused")]);
    let mut chat = agent_with_retries(Arc::clone(&model), 0)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    assert!(matches!(err, RunError::Model(ModelError::Transport(_))));
    assert!(!err.to_string().contains("retries"), "{err}");
    assert_eq!(run.outcome().unwrap().status, RunStatus::Failed);
    assert_eq!(model.requests().len(), 1);
    assert!(announced(&rec).is_empty());
}

/// A non-transient error (a 400) is never retried, even with retries configured.
#[tokio::test]
async fn a_protocol_error_is_never_retried() {
    let model = ScriptedModel::new([
        Reply::Protocol("HTTP 400: bad request".into()),
        text("unused"),
    ]);
    let mut chat = agent_with_retries(Arc::clone(&model), 3)
        .open_session()
        .await
        .unwrap();
    let rec = Recorder::default();
    let mut run = chat.start_turn("go").unwrap();

    let err = run.run(&rec).await.unwrap_err();

    assert!(matches!(err, RunError::Model(ModelError::Protocol(_))));
    assert_eq!(model.requests().len(), 1);
    assert!(announced(&rec).is_empty());
}
