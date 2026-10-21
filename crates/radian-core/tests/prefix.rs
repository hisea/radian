//! The interrupted-assistant prefix (A3).

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use radian_core::testing::{FailingJournal, RecordingJournal, SequenceLog, SlowJournal};
use radian_core::*;
use common::*;
use serde_json::json;

/// Opens a journaled session.
async fn session_with(
    model: Arc<ScriptedModel>,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, NoTools, ()> {
    // These tests pin the interrupted-prefix behaviour of one attempt; retrying (ADR 0017
    // ) would turn "one failure" into several, so it is off here.
    Arc::new(Agent::new(model, NoTools).with_config(AgentConfig {
        retries: 0,
        ..Default::default()
    }))
    .open_session_with_journal(Vec::new(), journal)
    .await
    .unwrap()
}

/// Records the text deltas in order; tests wait on it to cancel at the right moment.
#[derive(Clone, Default)]
struct DeltaSink {
    deltas: Arc<Mutex<Vec<String>>>,
}

impl EventSink for DeltaSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::ModelTextDelta { text } = event {
            self.deltas.lock().unwrap().push(text);
        }
    }
}

async fn until_deltas(seen: &Mutex<Vec<String>>, count: usize) {
    while seen.lock().unwrap().len() < count {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// Counts `after_model_response` invocations (M-B9: the prefix is not a complete response).
#[derive(Clone, Default)]
struct CountingHooks {
    after_model: Arc<AtomicUsize>,
}

impl Hooks for CountingHooks {
    fn after_model_response(
        &self,
        _cx: &HookContext,
        _response: &ModelResponse,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        self.after_model.fetch_add(1, Ordering::SeqCst);
        async { Ok(()) }
    }
}

use std::future::Future;

async fn hooked_session(
    model: Arc<ScriptedModel>,
    journal: Arc<dyn TranscriptJournal>,
    hooks: CountingHooks,
) -> AgentSession<Arc<ScriptedModel>, NoTools, CountingHooks> {
    Arc::new(Agent::new(model, NoTools).with_hooks(hooks))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

// ---------- AC-A3-2 / AC-A3-3 ----------

#[tokio::test(start_paused = true)]
async fn cancelling_mid_stream_commits_the_emitted_prefix() {
    let journal = Arc::new(RecordingJournal::new());
    let deltas = vec!["你😀好".to_owned(), ", ".to_owned(), "stream".to_owned()];
    let model = ScriptedModel::new([Reply::StreamHang(deltas.clone())]);
    let hooks = CountingHooks::default();
    let after_model = hooks.after_model.clone();
    let mut chat = hooked_session(model, journal.clone(), hooks).await;
    let sink = DeltaSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&sink), async {
        until_deltas(&seen, 2).await;
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    // The transcript ends with the prefix: the exact concatenation, no tool calls, flagged.
    let Message::Assistant {
        content,
        tool_calls,
        interrupted,
        ..
    } = chat.transcript().last().expect("a prefix message")
    else {
        panic!("no assistant at the end");
    };
    let expected: String = deltas.concat();
    assert_eq!(content, &vec![ContentPart::text(expected)]);
    assert!(tool_calls.is_empty());
    assert!(interrupted);

    // The journal holds exactly that message, and the event was emitted.
    let recorded = journal.messages();
    assert_eq!(recorded.last(), chat.transcript().last());
    assert!(
        journal
            .records()
            .iter()
            .any(|r| matches!(r, JournalRecord::Message { index: 1, .. }))
    );

    // The prefix is not a response: no after_model_response hook ran.
    assert_eq!(after_model.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn a_deadline_commits_the_prefix_the_same_way() {
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::StreamHang(vec!["half an ".into(), "answer".into()])]);
    let mut chat = session_with(model, journal.clone()).await;
    let options = TurnOptions {
        deadline: Some(tokio::time::Instant::now() + Duration::from_millis(50)),
        ..Default::default()
    };

    let outcome = chat.run_turn("go", options, &NoopSink).await.unwrap();

    assert_eq!(
        outcome.status,
        RunStatus::Cancelled(CancelReason::DeadlineExceeded)
    );
    let Message::Assistant { interrupted, .. } = chat.transcript().last().unwrap() else {
        panic!();
    };
    assert!(interrupted);
    assert_eq!(journal.messages().last(), chat.transcript().last());
    // F7: the outcome's final message is the prefix itself; hosts must check the flag.
    assert!(outcome.final_message.is_some_and(|m| matches!(
        &m,
        Message::Assistant {
            interrupted: true,
            ..
        }
    )));
}

// ---------- AC-A3-4 ----------

#[tokio::test(start_paused = true)]
async fn the_next_turn_carries_the_interrupted_prefix() {
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::StreamHang(vec!["partial".into()]), text("recovered")]);
    let mut chat = session_with(model.clone(), journal).await;
    let sink = DeltaSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&sink), async {
        until_deltas(&seen, 1).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    chat.run_turn("again", Default::default(), &NoopSink)
        .await
        .unwrap();

    // The second request carries user, assistant(interrupted), user - its own reply is
    // committed only after the response arrives.
    let second = &model.requests()[1];
    assert_eq!(second.messages.len(), 3);
    assert!(second.messages[0].is_user());
    assert!(matches!(
        &second.messages[1],
        Message::Assistant {
            interrupted: true,
            ..
        }
    ));
    assert!(second.messages[2].is_user());

    // The whole history stays valid for a fresh session.
    Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
        .open_session_with_history(chat.history_for_resume().history)
        .await
        .unwrap();
}

// ---------- AC-A3-5 ----------

#[tokio::test(start_paused = true)]
async fn no_deltas_means_no_prefix() {
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::StreamHang(Vec::new())]);
    let mut chat = session_with(model, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    // Only the user input; no empty assistant, in memory or in the journal.
    assert_eq!(chat.transcript().len(), 1);
    assert_eq!(journal.messages().len(), 1);
}

// ---------- AC-A3-7 ----------

#[tokio::test(start_paused = true)]
async fn each_model_call_accumulates_from_scratch() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let Reply::Respond(first) = calls(vec![call("c1", "a", json!({}))]) else {
        unreachable!()
    };
    let model = ScriptedModel::new([
        Reply::Streamed(vec!["A".into()], first),
        Reply::StreamHang(vec!["B".into()]),
    ]);
    let agent = Arc::new(Agent::new(model.clone(), tools));
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap();
    let sink = DeltaSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&sink), async {
        // Wait for the second model call's delta before cancelling.
        until_deltas(&seen, 2).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    // The prefix is only "B": the tool round-trip happened, but "A" belongs to the
    // completed first response, not to the cancelled call.
    let Message::Assistant {
        content,
        interrupted,
        ..
    } = chat.transcript().last().unwrap()
    else {
        panic!();
    };
    assert!(interrupted);
    assert_eq!(content, &vec![ContentPart::text("B")]);
}

// ---------- paths that must not commit (AC-A3-6) ----------

#[tokio::test(start_paused = true)]
async fn a_model_error_after_deltas_commits_no_prefix() {
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::StreamFail(vec!["doomed".into()], "boom".into())]);
    let mut chat = session_with(model, journal.clone()).await;

    let error = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(matches!(error, RunError::Model(_)), "{error:?}");
    assert_eq!(chat.transcript().len(), 1);
    assert_eq!(journal.messages().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_step_mid_stream_commits_nothing_and_stops_journaling() {
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::StreamHang(vec!["in flight".into()])]);
    let mut chat = session_with(model, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();

    let timed_out = tokio::time::timeout(Duration::from_millis(50), run.step(&NoopSink)).await;
    assert!(timed_out.is_err());
    let calls_at_drop = journal.calls();
    drop(run);

    assert!(chat.is_poisoned());
    assert_eq!(calls_at_drop, 1, "only the user input was journaled");
    assert_eq!(journal.calls(), 1, "the Drop path journaled something");
    // No prefix anywhere: memory holds only the input.
    assert!(chat.transcript().iter().skip(1).all(|m| !matches!(
        m,
        Message::Assistant {
            interrupted: true,
            ..
        }
    )));
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_tools_commits_no_prefix() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![(spec("a"), Behavior::Delay(60))]);
    let model = ScriptedModel::new([calls(vec![call("c1", "a", json!({}))])]);
    let agent = Arc::new(Agent::new(model, tools.clone()));
    let log = tools.log.clone();
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal.clone())
        .await
        .unwrap();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        until_started(&log, "c1").await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    assert!(!chat.transcript().iter().any(|m| matches!(
        m,
        Message::Assistant {
            interrupted: true,
            ..
        }
    )));
    assert!(!journal.messages().iter().any(|m| matches!(
        m,
        Message::Assistant {
            interrupted: true,
            ..
        }
    )));
}

async fn until_started(log: &ToolLog, id: &str) {
    while !log.started().iter().any(|s| s == id) {
        tokio::task::yield_now().await;
    }
}

// ---------- journal interaction (AC-A3-8) ----------

#[tokio::test(start_paused = true)]
async fn a_failed_prefix_append_fails_the_turn() {
    let journal = Arc::new(FailingJournal::fail_at(2)); // 1: user, 2: the prefix
    let model = ScriptedModel::new([Reply::StreamHang(vec!["half".into()])]);
    let mut chat = session_with(model, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (error, ()) = tokio::join!(run.run(&NoopSink), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    drop(run);

    assert!(matches!(error.unwrap_err(), RunError::Journal(_)));
    assert_eq!(
        chat.transcript().len(),
        1,
        "the prefix must not reach memory"
    );
    assert!(matches!(
        chat.start_turn("no"),
        Err(SessionError::RecoveryRequired)
    ));
    // What did land is enough to reopen.
    let history = radian_core::recover(&journal.records()).unwrap();
    Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
        .open_session_with_history(history)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_prefix_append_is_not_cancelled_by_the_cancellation() {
    let log = SequenceLog::default();
    let journal = Arc::new(SlowJournal::with_log(
        Duration::from_millis(100),
        log.clone(),
    ));
    let model = ScriptedModel::new([Reply::StreamHang(vec!["half".into()])]);
    let mut chat = session_with(model, journal.clone()).await;
    let sink = DeltaSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    // Cancel once the text is out: the prefix append then begins as part of convergence,
    // finds the cancellation already set, and must still run to completion (D3).
    let (outcome, cancelled_at) = tokio::join!(run.run(&sink), async {
        until_deltas(&seen, 1).await;
        let at = tokio::time::Instant::now();
        cancel.cancel();
        at
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    // The append the cancellation found in flight completed: the message is durable and in
    // memory, and convergence waited the append out (D3) - at least its full delay.
    assert_eq!(journal.calls(), 2);
    let Message::Assistant { interrupted, .. } = chat.transcript().last().unwrap() else {
        panic!();
    };
    assert!(interrupted);
    assert!(cancelled_at.elapsed() >= Duration::from_millis(100));
    assert!(cancelled_at.elapsed() <= Duration::from_millis(160));
}

#[tokio::test(start_paused = true)]
async fn prefix_failure_sweep_recovers_every_position() {
    // Turn 1 completes; turn 2 streams and gets cancelled. Four appends in total.
    let baseline = || {
        ScriptedModel::new([
            text("first"),
            Reply::StreamHang(vec!["second ".into(), "half".into()]),
        ])
    };
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with(baseline(), journal.clone()).await;
    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();
    let sink = DeltaSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("again").unwrap();
    let cancel = run.cancellation_handle();
    let (second, ()) = tokio::join!(run.run(&sink), async {
        until_deltas(&seen, 1).await;
        cancel.cancel();
    });
    assert_eq!(
        second.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    // user, assistant("first"), user, prefix.
    assert_eq!(journal.calls(), 4, "{:?}", journal.records());
    drop(chat);

    for nth in 1..=4 {
        let journal = Arc::new(FailingJournal::fail_at(nth));
        let mut chat = session_with(baseline(), journal.clone()).await;
        let turn1 = chat.run_turn("go", Default::default(), &NoopSink).await;
        let error = match turn1 {
            Err(e) => e,
            Ok(_) => {
                let mut run = chat.start_turn("again").unwrap();
                let cancel = run.cancellation_handle();
                let (result, ()) = tokio::join!(run.run(&NoopSink), async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    cancel.cancel();
                });
                drop(run);
                result.unwrap_err()
            }
        };
        assert!(
            matches!(error, RunError::Journal(_)),
            "append {nth}: {error:?}"
        );
        assert!(matches!(
            chat.start_turn("no"),
            Err(SessionError::RecoveryRequired)
        ));
        // Memory equals the journaled messages, and the journal reopens.
        let history = radian_core::recover(&journal.records()).unwrap();
        let expected: Vec<Message> = journal
            .records()
            .iter()
            .filter_map(|r| match r {
                JournalRecord::Message { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(chat.transcript(), expected, "append {nth}");
        Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
            .open_session_with_history(history)
            .await
            .unwrap();
    }
}

// ---------- R1: the reasoning prefix (AC-R1-2/3/4/5/9) ----------

/// Records reasoning deltas in order, so a test can cancel at the right moment.
#[derive(Clone, Default)]
struct ReasoningSink {
    deltas: Arc<Mutex<Vec<String>>>,
}

impl EventSink for ReasoningSink {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::ModelReasoningDelta { text } = event {
            self.deltas.lock().unwrap().push(text);
        }
    }
}

async fn until_reasoning(seen: &Mutex<Vec<String>>, count: usize) {
    while seen.lock().unwrap().len() < count {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(start_paused = true)]
async fn cancelling_during_reasoning_only_commits_the_reasoning_prefix() {
    // AC-R1-2, the regression test for the dogfooded bug: the model emits only reasoning
    // (`delta.content` stays empty), and cancellation must still commit what was produced.
    let journal = Arc::new(RecordingJournal::new());
    let deltas = vec!["Let me ".to_owned(), "think".to_owned()];
    let model = ScriptedModel::new([Reply::ReasoningStreamHang(deltas.clone())]);
    let mut chat = session_with(model, journal.clone()).await;
    let sink = ReasoningSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&sink), async {
        until_reasoning(&seen, 2).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    let expected = deltas.concat();
    let Message::Assistant {
        content,
        reasoning,
        tool_calls,
        interrupted,
        ..
    } = chat.transcript().last().expect("a prefix message")
    else {
        panic!("no assistant at the end");
    };
    assert!(
        content.is_empty(),
        "no visible text was emitted: {content:?}"
    );
    assert_eq!(reasoning, &vec![ContentPart::text(expected)]);
    assert!(tool_calls.is_empty());
    assert!(interrupted);
    // The journal holds exactly that message, field for field.
    assert_eq!(journal.messages().last(), chat.transcript().last());
}

#[tokio::test(start_paused = true)]
async fn cancelling_mid_stream_keeps_text_and_reasoning_separate() {
    // AC-R1-3: both channels produce a prefix, and neither leaks into the other.
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::BothStreamHang {
        text: vec!["visible ".into(), "answer".into()],
        reasoning: vec!["hidden ".into(), "thinking".into()],
    }]);
    let mut chat = session_with(model, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    let Message::Assistant {
        content, reasoning, ..
    } = chat.transcript().last().unwrap()
    else {
        panic!();
    };
    assert_eq!(content, &vec![ContentPart::text("visible answer")]);
    assert_eq!(reasoning, &vec![ContentPart::text("hidden thinking")]);
}

#[tokio::test(start_paused = true)]
async fn neither_text_nor_reasoning_means_no_prefix() {
    // AC-R1-4 / revised F4: only when *both* channels are empty is nothing committed.
    let journal = Arc::new(RecordingJournal::new());
    let model = ScriptedModel::new([Reply::BothStreamHang {
        text: Vec::new(),
        reasoning: Vec::new(),
    }]);
    let mut chat = session_with(model, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    assert_eq!(chat.transcript().len(), 1);
    assert_eq!(journal.messages().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn each_model_call_accumulates_reasoning_from_scratch() {
    // AC-R1-5: the first call's reasoning must not leak into the second call's prefix.
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let Reply::Respond(first) = calls(vec![call("c1", "a", json!({}))]) else {
        unreachable!()
    };
    let model = ScriptedModel::new([
        Reply::ReasoningStreamed(vec!["first ".into()], first),
        Reply::ReasoningStreamHang(vec!["second".into()]),
    ]);
    let agent = Arc::new(Agent::new(model.clone(), tools));
    let mut chat = agent
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap();
    let sink = ReasoningSink::default();
    let seen = sink.deltas.clone();
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&sink), async {
        until_reasoning(&seen, 2).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);

    // The completed first response kept its own reasoning...
    let completed = chat.transcript().iter().find_map(|m| match m {
        Message::Assistant {
            reasoning,
            interrupted: false,
            ..
        } if !reasoning.is_empty() => Some(reasoning.clone()),
        _ => None,
    });
    assert_eq!(completed, Some(vec![ContentPart::text("first ")]));
    // ...and the prefix holds only the second call's.
    let Message::Assistant {
        content,
        reasoning,
        interrupted,
        ..
    } = chat.transcript().last().unwrap()
    else {
        panic!();
    };
    assert!(interrupted);
    assert!(content.is_empty(), "{content:?}");
    assert_eq!(reasoning, &vec![ContentPart::text("second")]);
}

#[tokio::test(start_paused = true)]
async fn reasoning_deltas_write_no_journal_records_of_their_own() {
    // AC-R1-9: the event is a live projection (ADR 0010); only the committed message's
    // `reasoning` field is durable.
    let journal = Arc::new(RecordingJournal::new());
    let Reply::Respond(response) = text("done") else {
        unreachable!()
    };
    let model = ScriptedModel::new([Reply::ReasoningStreamed(
        vec!["a".into(), "b".into()],
        response,
    )]);
    let mut chat = session_with(model, journal.clone()).await;
    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    // user + assistant only: the two reasoning deltas added no records.
    let messages = journal.messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        journal
            .records()
            .iter()
            .all(|r| matches!(r, JournalRecord::Message { .. }))
    );
    let Message::Assistant { reasoning, .. } = &messages[1] else {
        panic!();
    };
    assert_eq!(reasoning, &vec![ContentPart::text("ab")]);
}
