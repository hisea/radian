//! The journal seam: ordering, full coverage of write points, failure semantics.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::testing::{FailingJournal, RecordingJournal, SequenceLog, SlowJournal};
use agent_core::*;
use common::*;
use serde_json::json;

/// Opens a session whose every transcript change is journaled to `journal`.
async fn session_with(
    model: Arc<ScriptedModel>,
    tools: MemTools,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, MemTools, ()> {
    Arc::new(Agent::new(model, tools))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

/// Notes `event:<index>` for every `TranscriptAppended` into a shared log, so tests can
/// interleave journal and event order.
#[derive(Clone)]
struct NotingSink {
    log: SequenceLog,
}

impl EventSink for NotingSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::TranscriptAppended { index, .. } = event {
            self.log.note(format!("event:{index}"));
        }
    }
}

// ---------- the sweep (AC-A1-3) ----------

/// The scripted conversation of the sweep: turn 1 covers a user input, steering, a parallel
/// two-call batch, a preflight failure and a budget synthesis; turn 2 adds a final answer.
fn sweep_script() -> (Vec<Reply>, Vec<Reply>) {
    let turn1 = vec![
        calls(vec![
            call("c1", "a", json!({"n": 1})),
            call("c2", "a", json!({"n": 2})),
        ]),
        calls(vec![call("c3", "missing", json!({}))]),
        calls(vec![call("c4", "a", json!({"n": 4}))]),
    ];
    let turn2 = vec![text("done")];
    (turn1, turn2)
}

fn sweep_tools() -> MemTools {
    // Delayed, so both batch calls are really dispatched before either settles.
    MemTools::new(vec![(shared("a"), Behavior::Delay(5))])
}

/// Runs the whole scripted conversation once without failures, returning the exact journal
/// (messages and markers).
async fn baseline() -> Vec<JournalRecord> {
    let (t1, t2) = sweep_script();
    let mut script = t1;
    script.extend(t2);
    let model = ScriptedModel::new(script);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with(model, sweep_tools(), journal.clone()).await;
    let options = TurnOptions {
        max_model_calls: Some(3),
        ..Default::default()
    };
    let mut run = chat.start_turn_with("go", options).unwrap();
    run.steering_handle().steer("more").unwrap();
    run.run(&NoopSink).await.unwrap();
    drop(run);
    chat.run_turn("again", Default::default(), &NoopSink)
        .await
        .unwrap();
    journal.records()
}

/// How many tools the original run had dispatched once `records` is all that succeeded:
/// every started marker, plus the call whose settle is the record that just failed.
fn started_tools_after(records: &[JournalRecord]) -> usize {
    records
        .iter()
        .filter(|r| matches!(r, JournalRecord::ToolExecutionStarted { .. }))
        .count()
}

/// Every append position fails once; each failure leaves memory equal to the journaled
/// prefix, stops all later dispatch, and requires recovery before another turn.
#[tokio::test(start_paused = true)]
async fn journal_failure_sweep() {
    let records = baseline().await;
    let total = records.len();
    let messages: Vec<Message> = records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        messages.len(),
        11,
        "the scripted conversation must append 11 messages"
    );
    assert_eq!(
        total, 15,
        "plus four execution markers for the two-call batch"
    );

    for nth in 1..=total {
        let (t1, t2) = sweep_script();
        let mut script = t1;
        script.extend(t2);
        let model = ScriptedModel::new(script);
        let tools = sweep_tools();
        let log = tools.log.clone();
        let journal = Arc::new(FailingJournal::fail_at(nth));
        let mut chat = session_with(model, tools, journal.clone()).await;
        let options = TurnOptions {
            max_model_calls: Some(3),
            ..Default::default()
        };

        let mut run = chat.start_turn_with("go", options).unwrap();
        run.steering_handle().steer("more").unwrap();
        let first = run.run(&NoopSink).await;
        drop(run);
        // Turn 2 only starts when turn 1 survived its appends.
        let error = match first {
            Err(e) => e,
            Ok(_) => chat
                .run_turn("again", Default::default(), &NoopSink)
                .await
                .expect_err("the failure must land in one of the two turns"),
        };
        assert!(
            matches!(error, RunError::Journal(ref e) if e.0.contains("injected")),
            "append {nth}: {error:?}"
        );

        // Memory is exactly the journaled message prefix of the baseline.
        let succeeded = &records[..nth - 1];
        let expected: Vec<Message> = succeeded
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            chat.transcript(),
            expected,
            "append {nth}: transcript must equal the messages of the first {} records",
            nth - 1
        );
        assert_eq!(journal.records().len(), nth - 1);

        // Nothing new executes at or after the failed append. When the failure is a
        // started-marker append, the launch loop stops before polling the just-pushed
        // futures, so only calls that already settled have executed; otherwise every
        // dispatched call of this script has run.
        let expected_started =
            if matches!(records[nth - 1], JournalRecord::ToolExecutionStarted { .. }) {
                succeeded
                    .iter()
                    .filter(|r| matches!(r, JournalRecord::ToolExecutionSettled { .. }))
                    .count()
            } else {
                started_tools_after(succeeded)
            };
        assert_eq!(
            log.started().len(),
            expected_started,
            "append {nth}: {:?}",
            log.started()
        );

        assert!(matches!(
            chat.start_turn("no"),
            Err(SessionError::RecoveryRequired)
        ));
        chat.close().await.unwrap();

        // Whatever succeeded is enough to reopen: recover classifies every open call from
        // the markers (settled -> its result, started only -> outcome unknown, else ->
        // not started) and the result validates as history.
        let history = agent_core::recover(&journal.records()).unwrap();
        Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
            .open_session_with_history(history)
            .await
            .unwrap();
    }
}

// ---------- S4 (AC-A1-4) ----------

#[tokio::test(start_paused = true)]
async fn an_assistant_append_failure_dispatches_no_tools() {
    let model = ScriptedModel::new([calls(vec![
        call("c1", "a", json!({})),
        call("c2", "a", json!({})),
    ])]);
    let tools = MemTools::new(vec![(shared("a"), Behavior::Echo)]);
    let log = tools.log.clone();
    let journal = Arc::new(FailingJournal::fail_at(2)); // 1: user, 2: assistant
    let mut chat = session_with(model, tools, journal).await;

    let error = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(matches!(error, RunError::Journal(_)), "{error:?}");
    assert_eq!(log.started(), Vec::<String>::new());
    assert_eq!(chat.transcript().len(), 1);
    assert!(chat.close().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_result_append_failure_runs_the_tool_once_and_starts_no_more() {
    // `a` is Exclusive, so call 2 cannot start before call 1's result is committed.
    let model = ScriptedModel::new([calls(vec![
        call("c1", "a", json!({})),
        call("c2", "a", json!({})),
    ])]);
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let log = tools.log.clone();
    // 1: user, 2: assistant, 3: started, 4: settled, 5: c1's result message.
    let journal = Arc::new(FailingJournal::fail_at(5));
    let mut chat = session_with(model, tools, journal).await;

    let error = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(matches!(error, RunError::Journal(_)), "{error:?}");
    assert_eq!(log.started(), ["c1"]);
    // The assistant message is the last thing in memory; c2 never started.
    assert_eq!(chat.transcript().len(), 2);
    assert!(matches!(
        chat.start_turn("no"),
        Err(SessionError::RecoveryRequired)
    ));
}

// ---------- D3 (AC-A1-5) ----------

/// Cancelling during an append still lets that append land; the turn converges only after.
#[tokio::test(start_paused = true)]
async fn cancellation_waits_for_the_inflight_append() {
    let model = ScriptedModel::new([calls(vec![call("c1", "a", json!({"n": 1}))]), text("done")]);
    let tools = MemTools::new(vec![(spec("a"), Behavior::Delay(10))]);
    let log = SequenceLog::default();
    let journal = Arc::new(SlowJournal::with_log(
        Duration::from_millis(100),
        log.clone(),
    ));
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, cancelled_at) = tokio::join!(run.run(&NoopSink), async {
        // Wait until the tool result's append is in flight (records: user=0, assistant=1,
        // result=2), then cancel right into it.
        while !log.entries().iter().any(|e| e == "journal-start:2") {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let at = tokio::time::Instant::now();
        cancel.cancel();
        at
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    // The in-flight append completed and reached memory before the turn converged:
    // user, assistant, started, settled and the result message.
    assert_eq!(journal.calls(), 5);
    assert_eq!(journal.records().len(), 5);
    assert_eq!(result_ids(chat.transcript()), ["c1"]);
    // Convergence waited for the in-flight append (up to its 100 ms) plus little else.
    assert!(
        cancelled_at.elapsed() <= Duration::from_millis(150),
        "{:?}",
        cancelled_at.elapsed()
    );
}

// ---------- backpressure and parallel batches (AC-A1-6) ----------

#[tokio::test(start_paused = true)]
async fn a_parallel_batch_completes_over_a_slow_journal() {
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "a", json!({})),
            call("s2", "a", json!({})),
            call("s3", "a", json!({})),
            call("s4", "a", json!({})),
        ]),
        text("done"),
    ]);
    let tools = MemTools::new(vec![(shared("a"), Behavior::Delay(5))]);
    let log = tools.log.clone();
    let journal = Arc::new(SlowJournal::new(Duration::from_millis(20)));
    let mut chat = session_with(model, tools, journal.clone()).await;

    let outcome = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    // The calls really ran concurrently despite the journal awaits between completions.
    assert_eq!(log.max_concurrent(), 4);
    // Results still land in call order.
    assert_eq!(result_ids(chat.transcript()), ["s1", "s2", "s3", "s4"]);
    // user, assistant, 4x (started, settled), 4 results, final
    assert_eq!(journal.records().len(), 15);
}

// ---------- D7 (AC-A1-7) ----------

#[tokio::test(start_paused = true)]
async fn dropping_a_step_never_touches_the_journal_again() {
    let model = ScriptedModel::new([calls(vec![call("h1", "a", json!({}))])]);
    let tools = MemTools::new(vec![(spec("a"), Behavior::Hang)]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();

    // Let the input and assistant message commit, then hang inside the tool.
    let timed_out = tokio::time::timeout(Duration::from_millis(50), run.step(&NoopSink)).await;
    assert!(timed_out.is_err());
    let calls_at_drop = journal.calls();

    drop(run);
    assert!(chat.transcript().len() >= 2);
    assert!(chat.is_poisoned());
    // The Drop path repaired memory only: no further journal append.
    assert_eq!(journal.calls(), calls_at_drop);
    let replay: Vec<Message> = journal
        .records()
        .iter()
        .filter_map(|r| match r {
            JournalRecord::Message { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    let transcript = chat.transcript();
    let n = replay.len();
    assert!(transcript.starts_with(&replay), "{transcript:?}");
    for message in &transcript[n..] {
        assert!(matches!(message, Message::Tool { result }
            if result.error_code.as_deref() == Some("interrupted")));
    }

    // The repaired history is a legal one to resume from.
    let history = chat.history_for_resume().history;
    let model2 = ScriptedModel::new([text("fine")]);
    let tools2 = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    Arc::new(Agent::new(model2, tools2))
        .open_session_with_history(history)
        .await
        .unwrap();
}

// ---------- S5 (AC-A1-8) ----------

#[tokio::test(start_paused = true)]
async fn events_mirror_the_journal_in_order() {
    let model = ScriptedModel::new([calls(vec![call("c1", "a", json!({}))]), text("done")]);
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let log = SequenceLog::default();
    let journal = Arc::new(RecordingJournal::with_log(log.clone()));
    let mut chat = session_with(model, tools, journal.clone()).await;
    let sink = NotingSink { log: log.clone() };

    let mut run = chat.start_turn("go").unwrap();
    run.steering_handle().steer("extra").unwrap();
    run.run(&sink).await.unwrap();
    drop(run);

    let replay = journal.messages();
    assert_eq!(replay, chat.transcript(), "journal replays the transcript");
    // Every index appears exactly once as a journal entry and once as an event, in index
    // order, and the journal entry comes first.
    let mut expected = Vec::new();
    for index in 0..replay.len() {
        expected.push(format!("journal:{index}"));
        expected.push(format!("event:{index}"));
    }
    let entries: Vec<String> = log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("journal:") || e.starts_with("event:"))
        // Marker records (A2) interleave too; this test is about message order.
        .filter(|e| !e.contains("started:") && !e.contains("settled:"))
        .collect();
    assert_eq!(entries, expected);
}

// ---------- D8 (AC-A1-9) ----------

#[tokio::test(start_paused = true)]
async fn history_is_not_rejournaled() {
    let history = vec![
        Message::user("earlier"),
        Message::Assistant {
            content: vec![ContentPart::text("earlier answer")],
            tool_calls: vec![],
            provider_payload: None,
            interrupted: false,
        },
    ];
    let model = ScriptedModel::new([text("fresh")]);
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = Arc::new(Agent::new(model, tools))
        .open_session_with_journal(history, journal.clone())
        .await
        .unwrap();

    chat.run_turn("hi", Default::default(), &NoopSink)
        .await
        .unwrap();

    let records = journal.records();
    assert_eq!(records.len(), 2); // the new input and the new answer, nothing else
    assert_eq!(
        records[0],
        JournalRecord::Message {
            index: 2,
            message: Message::user("hi"),
        }
    );
    assert!(matches!(
        &records[1],
        JournalRecord::Message { index: 3, .. }
    ));
}

// ---------- steering is never lost to a journal failure ----------

/// Two steering messages are queued before the first step. Appends: 1 = input, 2 = first
/// steering message, 3 = second. Whatever fails, every steering message is either in the
/// journal or handed back to the host.
#[tokio::test(start_paused = true)]
async fn steering_that_fails_to_journal_is_handed_back_not_lost() {
    for (fail_at, journaled) in [(2, 0), (3, 1)] {
        let model = ScriptedModel::new([text("unused")]);
        let journal = Arc::new(FailingJournal::fail_at(fail_at));
        let mut chat = session_with(model, MemTools::new(vec![]), journal.clone()).await;
        let mut run = chat.start_turn("go").unwrap();
        run.steering_handle().steer("s1").unwrap();
        run.steering_handle().steer("s2").unwrap();

        let error = run.run(&NoopSink).await.unwrap_err();

        assert!(matches!(error, RunError::Journal(_)), "{error:?}");
        let expected: Vec<Message> = ["s1", "s2"][journaled..]
            .iter()
            .map(|t| Message::from(*t))
            .collect();
        let outcome = run.outcome().expect("the turn ended");
        assert_eq!(outcome.unapplied_steering, expected, "fail_at {fail_at}");
        drop(run);
        // The same messages are available to a host that only has the session.
        assert_eq!(
            chat.history_for_resume().unapplied_steering,
            expected,
            "fail_at {fail_at}"
        );
        assert_eq!(chat.transcript().len(), 1 + journaled);
    }
}
