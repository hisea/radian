//! Tool execution markers and the recovery rules (A2).

mod common;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use radian_core::testing::{FailingJournal, RecordingJournal, SequenceLog, SlowJournal};
use radian_core::*;
use common::*;
use serde_json::json;

/// Opens a session whose every transcript change is journaled to `journal`.
async fn session_with(
    model: Arc<ScriptedModel>,
    tools: impl ToolProvider,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, impl ToolProvider, ()> {
    Arc::new(Agent::new(model, tools))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

fn started(id: &str, name: &str, arguments: serde_json::Value) -> JournalRecord {
    JournalRecord::ToolExecutionStarted {
        call_id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn settled(id: &str, text: &str) -> JournalRecord {
    JournalRecord::ToolExecutionSettled {
        call_id: id.into(),
        result: ToolResult::success(id, vec![ContentPart::text(text)]),
    }
}

fn message(index: usize, text: &str) -> JournalRecord {
    JournalRecord::Message {
        index,
        message: Message::user(text),
    }
}

// ---------- replay / repair / recover as pure functions (AC-A2-10) ----------

#[test]
fn replay_rejects_non_contiguous_indexes() {
    use radian_core::ReplayError;
    // A gap ...
    let records = [message(0, "a"), message(2, "c")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 1,
            found: 2
        })
    );
    // ... a repeat ...
    let records = [message(0, "a"), message(1, "b"), message(1, "b")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 2,
            found: 1
        })
    );
    // ... and a wrong start.
    let records = [message(1, "a")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 0,
            found: 1
        })
    );
}

#[test]
fn replay_ignores_markers_and_accepts_empty_input() {
    let records = [
        message(0, "a"),
        started("c1", "t", json!({})),
        message(1, "b"),
        settled("c1", "ok"),
    ];
    assert_eq!(
        replay(&records).unwrap(),
        vec![Message::user("a"), Message::user("b")]
    );
    assert_eq!(replay(&[]).unwrap(), Vec::<Message>::new());
}

#[test]
fn repair_fills_unpaired_calls_by_the_three_rules() {
    let history = vec![
        Message::user("go"),
        Message::Assistant {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![
                call("ran", "a", json!({})),
                call("half", "a", json!({})),
                call("never", "a", json!({})),
            ],
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        },
        // The `ran` result message is already there.
        Message::Tool {
            result: ToolResult::success("ran", vec![ContentPart::text("done")]),
            meta: MessageMeta::default(),
        },
    ];
    let assistant = history[1].clone();
    let records = [
        message(0, "go"),
        JournalRecord::Message {
            index: 1,
            message: assistant,
        },
        started("ran", "a", json!({})),
        started("half", "a", json!({})),
        settled("ran", "done"),
    ];

    let fill = repair(&history, &records);

    assert_eq!(fill.len(), 2);
    // `half` only started: the outcome is unknown and must not be assumed unexecuted.
    let half = result_for(&fill, "half");
    assert_eq!(code(half), Some("outcome_unknown_after_crash"));
    assert!(half.is_error);
    assert!(half.text().to_lowercase().contains("unknown"));
    // `never` never started.
    let never = result_for(&fill, "never");
    assert_eq!(code(never), Some("not_started_after_crash"));
    assert!(never.is_error);

    // `recover` produces a complete, valid history from the records alone.
    let mut recovered = replay(&records).unwrap();
    recovered.extend(repair(&recovered, &records));
    assert!(matches!(
        &recovered[3],
        Message::Tool { result, .. } if result.call_id == "half"
    ));
}

#[test]
fn repair_fills_from_settled_exactly_and_leaves_complete_history_alone() {
    let history = vec![
        Message::user("go"),
        Message::Assistant {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![call("c1", "a", json!({}))],
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        },
    ];
    let records = [
        message(0, "go"),
        message(1, ""),
        started("c1", "a", json!({"final": 1})),
        settled("c1", "the settled text"),
    ];
    let fill = repair(&history, &records);
    assert_eq!(
        fill,
        vec![Message::Tool {
            result: ToolResult::success("c1", vec![ContentPart::text("the settled text")]),
            meta: MessageMeta::default(),
        }]
    );

    // A history that already answers every call needs nothing.
    let complete = vec![history[0].clone(), history[1].clone(), fill[0].clone()];
    assert_eq!(repair(&complete, &records), Vec::<Message>::new());
}

#[tokio::test(start_paused = true)]
async fn recover_yields_history_that_validates() {
    let records = [
        message(0, "go"),
        JournalRecord::Message {
            index: 1,
            message: Message::Assistant {
                reasoning: Vec::new(),
                content: vec![],
                tool_calls: vec![call("c1", "a", json!({})), call("c2", "a", json!({}))],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
        },
        started("c1", "a", json!({})),
    ];
    let history = recover(&records).unwrap();
    // The recovered history opens a new session, i.e. passes the real validation.
    Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
        .open_session_with_history(history.clone())
        .await
        .unwrap();
    assert_eq!(result_ids(&history), ["c1", "c2"]);
    assert_eq!(
        code(result_for(&history, "c1")),
        Some("outcome_unknown_after_crash")
    );
    assert_eq!(
        code(result_for(&history, "c2")),
        Some("not_started_after_crash")
    );
}

// ---------- Core wiring: Started / Settled (AC-A2-2/3/4) ----------

/// A tool that checks, on the first line of `execute`, that its own `Started` marker (with
/// the exact final arguments) is already durable in the journal.
#[derive(Clone)]
struct ProbeTools {
    journal: Arc<RecordingJournal>,
    checks: Arc<Mutex<Vec<(String, bool)>>>,
    specs: Arc<Vec<ToolSpec>>,
}

struct ProbeSession {
    journal: Arc<RecordingJournal>,
    checks: Arc<Mutex<Vec<(String, bool)>>>,
    specs: Vec<ToolSpec>,
}

impl ToolProvider for ProbeTools {
    type Session = ProbeSession;

    async fn open_session(&self) -> Result<ProbeSession, ToolError> {
        Ok(ProbeSession {
            journal: Arc::clone(&self.journal),
            checks: Arc::clone(&self.checks),
            specs: self.specs.iter().cloned().collect(),
        })
    }
}

impl ToolSession for ProbeSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let seen = self.journal.records().iter().any(|r| match r {
            JournalRecord::ToolExecutionStarted {
                call_id,
                name,
                arguments,
            } => call_id == &call.id && name == &call.name && arguments == &call.arguments,
            _ => false,
        });
        self.checks.lock().unwrap().push((call.id.clone(), seen));
        Ok(ToolOutput::text(call.arguments.to_string()))
    }
}

/// Rewrites the arguments of calls to `rw`.
#[derive(Clone, Copy)]
struct Rewrite;

impl Hooks for Rewrite {
    fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        let decision = if call.name == "rw" {
            ToolDecision::Rewrite {
                arguments: json!({"rewritten": true}),
            }
        } else {
            ToolDecision::Allow
        };
        async move { Ok(decision) }
    }
}

async fn session_with_hooks(
    model: Arc<ScriptedModel>,
    tools: impl ToolProvider,
    journal: Arc<dyn TranscriptJournal>,
    hooks: impl Hooks,
) -> AgentSession<Arc<ScriptedModel>, impl ToolProvider, impl Hooks> {
    Arc::new(Agent::new(model, tools).with_hooks(hooks))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

/// AC-A2-2: serial, parallel-shared, exclusive and rewritten calls all see their own
/// `Started` (with the rewritten arguments) before their body starts.
#[tokio::test(start_paused = true)]
async fn execution_starts_only_after_started_is_durable() {
    let journal = Arc::new(RecordingJournal::new());
    let probe = ProbeTools {
        journal: Arc::clone(&journal),
        checks: Arc::default(),
        specs: Arc::new(vec![shared("a"), spec("b"), spec("rw")]),
    };
    let checks = probe.checks.clone();
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "a", json!({"n": 1})),
            call("s2", "a", json!({"n": 2})),
            call("x1", "b", json!({})),
            call("r1", "rw", json!({"n": 3})),
        ]),
        text("done"),
    ]);
    let mut chat = session_with_hooks(model, probe, journal.clone(), Rewrite).await;

    let outcome = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    // Every executed call saw its own Started, with the final (rewritten) arguments.
    assert_eq!(checks.lock().unwrap().len(), 4);
    for (id, seen) in checks.lock().unwrap().iter() {
        assert!(seen, "call {id} executed before its Started was durable");
    }
    let r1 = journal.records().iter().find_map(|r| match r {
        JournalRecord::ToolExecutionStarted {
            call_id, arguments, ..
        } if call_id == "r1" => Some(arguments.clone()),
        _ => None,
    });
    assert_eq!(r1, Some(json!({"rewritten": true})));
}

/// Appends a review note to every result.
#[derive(Clone, Copy)]
struct Review;

impl Hooks for Review {
    fn after_tool_call(
        &self,
        _cx: &HookContext,
        _call: &ToolCall,
        result: &mut ToolResult,
    ) -> impl Future<Output = Result<(), HookError>> + Send {
        result.content.push(ContentPart::text("reviewed"));
        async { Ok(()) }
    }
}

/// AC-A2-3: `Settled` precedes the result message and carries the final, post-hook result.
#[tokio::test(start_paused = true)]
async fn settled_precedes_and_matches_the_result_message() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![(shared("a"), Behavior::Echo)]);
    let model = ScriptedModel::new([
        calls(vec![call("c1", "a", json!({})), call("c2", "a", json!({}))]),
        text("done"),
    ]);
    let mut chat = session_with_hooks(model, tools, journal.clone(), Review).await;

    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    let records = journal.records();
    for id in ["c1", "c2"] {
        let settled_at = records.iter().position(|r| {
            matches!(r,
            JournalRecord::ToolExecutionSettled { call_id, .. } if call_id == id)
        });
        let message_at = records.iter().position(|r| {
            matches!(r,
            JournalRecord::Message { message: Message::Tool { result, .. }, .. }
                if result.call_id == id)
        });
        let (Some(settled_at), Some(message_at)) = (settled_at, message_at) else {
            panic!("no settled/message pair for {id}: {records:?}");
        };
        assert!(
            settled_at < message_at,
            "{id}: settled at {settled_at}, message at {message_at}"
        );
        // The settled result is exactly the one committed, hook modifications included.
        match (&records[settled_at], &records[message_at]) {
            (
                JournalRecord::ToolExecutionSettled {
                    result: settled, ..
                },
                JournalRecord::Message {
                    message: Message::Tool { result, .. },
                    ..
                },
            ) => assert_eq!(settled, result),
            other => panic!("unexpected records for {id}: {other:?}"),
        }
        assert!(settled_result_content_has(&records[settled_at], "reviewed"));
    }
}

fn settled_result_content_has(record: &JournalRecord, text: &str) -> bool {
    match record {
        JournalRecord::ToolExecutionSettled { result, .. } => result
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text: t, .. } if t.contains(text))),
        _ => false,
    }
}

/// Denies `d`, answers `r` itself, and fails the policy for `h`.
#[derive(Clone, Copy)]
struct Policy;

impl Hooks for Policy {
    fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        let decision = match call.name.as_str() {
            "d" => ToolDecision::Deny {
                reason: "not allowed".into(),
            },
            "r" => ToolDecision::Respond(ToolResult::success(
                &call.id,
                vec![ContentPart::text("answered locally")],
            )),
            "h" => return std::future::ready(Err(HookError::new("policy broke"))),
            _ => ToolDecision::Allow,
        };
        std::future::ready(Ok(decision))
    }
}

/// AC-A2-4: calls that never reach a tool session leave no markers behind.
#[tokio::test(start_paused = true)]
async fn unexecuted_calls_carry_no_markers() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![
        (spec("d"), Behavior::Echo),
        (typed("t"), Behavior::Echo),
    ]);
    let log = tools.log.clone();
    let model = ScriptedModel::new([
        calls(vec![
            call("u1", "missing", json!({})),            // unknown_tool
            call("t1", "t", json!({"x": "not an int"})), // invalid_arguments
            call("d1", "d", json!({})),                  // denied
            call("r1", "r", json!({})),                  // responded by the hook
            call("h1", "h", json!({})),                  // hook_error
        ]),
        text("done"),
    ]);
    let mut chat = session_with_hooks(model, tools, journal.clone(), Policy).await;

    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    // Nothing executed, so no markers at all.
    assert_eq!(log.started(), Vec::<String>::new());
    assert!(!journal.records().iter().any(|r| matches!(
        r,
        JournalRecord::ToolExecutionStarted { .. } | JournalRecord::ToolExecutionSettled { .. }
    )));
    // Each call has exactly one result message.
    for id in ["u1", "t1", "d1", "r1", "h1"] {
        assert_eq!(
            result_ids(chat.transcript())
                .iter()
                .filter(|i| *i == id)
                .count(),
            1
        );
    }
}

/// AC-A2-4, second half: `skipped_by_steering` and cancelled-before-start calls leave no
/// markers either.
#[tokio::test(start_paused = true)]
async fn skipped_and_cancelled_before_start_carry_no_markers() {
    // Steering skips the exclusive call that has not launched yet.
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![
        (shared("a"), Behavior::Delay(20)),
        (spec("b"), Behavior::Echo),
    ]);
    let model = ScriptedModel::new([
        calls(vec![call("s1", "a", json!({})), call("s2", "b", json!({}))]),
        // The injected steering keeps the turn going, so one more reply is needed.
        text("ok"),
    ]);
    let log = tools.log.clone();
    let mut chat = session_with(model.clone(), tools, journal.clone()).await;
    let mut run = chat
        .start_turn_with(
            "go",
            TurnOptions {
                steer_mode: SteerMode::SkipRemaining,
                ..Default::default()
            },
        )
        .unwrap();
    let steer = run.steering_handle();
    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        // Once s1 is running, a new message makes the not-yet-launched s2 skip.
        until_started(&log, "s1").await;
        steer.steer("stop").unwrap();
    });
    assert_eq!(outcome.unwrap().status, RunStatus::Completed);
    assert!(matches!(
        journal.records().iter().find(|r| matches!(r,
            JournalRecord::Message { message: Message::Tool { result, .. }, .. } if result.call_id == "s2")),
        Some(JournalRecord::Message { message: Message::Tool { result, .. }, .. }
            ) if result.error_code.as_deref() == Some("skipped_by_steering")));
    assert!(!journal.records().iter().any(|r| matches!(r,
        JournalRecord::ToolExecutionStarted { call_id, .. } if call_id == "s2")));

    // A cancel before the exclusive call starts.
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![
        (shared("a"), Behavior::Hang),
        (spec("b"), Behavior::Echo),
    ]);
    let model = ScriptedModel::new([calls(vec![
        call("h1", "a", json!({})),
        call("x1", "b", json!({})),
    ])]);
    let log = tools.log.clone();
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();
    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        until_started(&log, "h1").await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
    });
    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    assert!(!log.started().contains(&"x1".to_owned()));
    assert!(!journal.records().iter().any(|r| matches!(r,
        JournalRecord::ToolExecutionStarted { call_id, .. } if call_id == "x1")));
    assert_eq!(code(result_for(chat.transcript(), "x1")), Some("cancelled"));
}

async fn until_started(log: &ToolLog, id: &str) {
    while !log.started().iter().any(|s| s == id) {
        tokio::task::yield_now().await;
    }
}

// ---------- cancellation and drop semantics (AC-A2-7 / AC-A2-9) ----------

/// AC-A2-7: tools cancelled gracefully or dropped after the grace both settle as
/// `cancelled`, before their result messages.
#[tokio::test(start_paused = true)]
async fn cancelled_tools_settle_as_cancelled() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![
        (shared("clean"), Behavior::CleansUp(10)),
        (shared("stubborn"), Behavior::Stubborn),
    ]);
    let log = tools.log.clone();
    let model = ScriptedModel::new([calls(vec![
        call("g1", "clean", json!({})),
        call("s1", "stubborn", json!({})),
    ])]);
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        until_started(&log, "g1").await;
        until_started(&log, "s1").await;
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    drop(run);
    let records = journal.records();
    for id in ["g1", "s1"] {
        let settled_at = records.iter().position(|r| {
            matches!(r,
            JournalRecord::ToolExecutionSettled { call_id, result } if call_id == id
                && result.error_code.as_deref() == Some("cancelled"))
        });
        let message_at = records.iter().position(|r| matches!(r,
            JournalRecord::Message { message: Message::Tool { result, .. }, .. } if result.call_id == id));
        let (Some(settled_at), Some(message_at)) = (settled_at, message_at) else {
            panic!("no cancelled settle/message pair for {id}: {records:?}");
        };
        assert!(settled_at < message_at, "{id}: {records:?}");
    }
    // The graceful one used its cleanup window.
    let cleaned = log.cleaned_up.lock().unwrap().clone();
    assert_eq!(cleaned, ["g1"]);
}

/// AC-A2-7 + D3: a cancellation that lands while the `Settled` append is in flight waits
/// for it; the marker and the result message both land before the turn converges.
#[tokio::test(start_paused = true)]
async fn cancellation_waits_for_the_inflight_settled_append() {
    let log = SequenceLog::default();
    let journal = Arc::new(SlowJournal::with_log(
        Duration::from_millis(100),
        log.clone(),
    ));
    let tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
    let model = ScriptedModel::new([calls(vec![call("c1", "a", json!({}))])]);
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();
    let cancel = run.cancellation_handle();

    let (outcome, cancelled_at) = tokio::join!(run.run(&NoopSink), async {
        while !log
            .entries()
            .iter()
            .any(|e| e == "journal-start:settled:c1")
        {
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
    assert_eq!(journal.calls(), 5, "{:?}", journal.records());
    // The settled marker landed with the cancelled result right behind it.
    let records = journal.records();
    let settled_at = records
        .iter()
        .position(
            |r| matches!(r, JournalRecord::ToolExecutionSettled { call_id, .. } if call_id == "c1"),
        )
        .unwrap();
    let message_at = records
        .iter()
        .position(|r| matches!(r, JournalRecord::Message { message: Message::Tool { result, .. }, .. } if result.call_id == "c1"))
        .unwrap();
    assert!(settled_at < message_at);
    // Convergence waited out the in-flight settled append, then the result message
    // append: two slow appends after the cancel, plus little else.
    assert!(
        cancelled_at.elapsed() <= Duration::from_millis(250),
        "{:?}",
        cancelled_at.elapsed()
    );
}

/// AC-A2-9: dropping a step mid-tool leaves the call started but unsettled; the Drop path
/// does not touch the journal, and recovery classifies the call as outcome-unknown.
#[tokio::test(start_paused = true)]
async fn dropping_mid_tool_leaves_no_settled() {
    let journal = Arc::new(RecordingJournal::new());
    let tools = MemTools::new(vec![(spec("a"), Behavior::Hang)]);
    let model = ScriptedModel::new([calls(vec![call("h1", "a", json!({}))])]);
    let mut chat = session_with(model, tools, journal.clone()).await;
    let mut run = chat.start_turn("go").unwrap();

    let timed_out = tokio::time::timeout(Duration::from_millis(50), run.step(&NoopSink)).await;
    assert!(timed_out.is_err());
    let calls_at_drop = journal.calls();
    drop(run);

    assert!(chat.is_poisoned());
    // Exactly the three appends of the live path (user, assistant, started); the Drop
    // repair writes to memory only (E6/D7).
    assert_eq!(calls_at_drop, 3, "{:?}", journal.records());
    assert_eq!(journal.calls(), 3, "the Drop path journaled something");
    let records = journal.records();
    assert!(records.iter().any(|r| matches!(r,
        JournalRecord::ToolExecutionStarted { call_id, .. } if call_id == "h1")));
    assert!(!records.iter().any(|r| matches!(r,
        JournalRecord::ToolExecutionSettled { call_id, .. } if call_id == "h1")));

    // Recovery reads the markers: started without settled means the outcome is unknown.
    let history = recover(&records).unwrap();
    assert_eq!(
        code(result_for(&history, "h1")),
        Some("outcome_unknown_after_crash")
    );
    Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
        .open_session_with_history(history)
        .await
        .unwrap();
}

// ---------- parallel batches under a slow journal (AC-A2-8) ----------

/// AC-A2-8: with a slow journal, the started markers come in dispatch order, the result
/// messages in call order, and the batch still completes.
#[tokio::test(start_paused = true)]
async fn started_markers_follow_dispatch_and_results_follow_call_order() {
    let journal = Arc::new(SlowJournal::new(Duration::from_millis(20)));
    let tools = MemTools::new(vec![(shared("a"), Behavior::Delay(5))]);
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "a", json!({})),
            call("s2", "a", json!({})),
            call("s3", "a", json!({})),
            call("s4", "a", json!({})),
        ]),
        text("done"),
    ]);
    let log = tools.log.clone();
    let mut chat = session_with(model, tools, journal.clone()).await;

    let outcome = chat
        .run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(log.max_concurrent(), 4);
    // Started markers in dispatch order ...
    let started_order: Vec<String> = journal
        .records()
        .iter()
        .filter_map(|r| match r {
            JournalRecord::ToolExecutionStarted { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started_order, ["s1", "s2", "s3", "s4"]);
    // ... results in call order, settled or not in completion order.
    assert_eq!(result_ids(chat.transcript()), ["s1", "s2", "s3", "s4"]);
}

// ---------- the crash traversal (AC-A2-5) ----------

/// Denies every call to `d`.
#[derive(Clone, Copy)]
struct DenyD;

impl Hooks for DenyD {
    fn before_tool_call(
        &self,
        _cx: &HookContext,
        call: &ToolCall,
    ) -> impl Future<Output = Result<ToolDecision, HookError>> + Send {
        let decision = if call.name == "d" {
            ToolDecision::Deny {
                reason: "not in this test".into(),
            }
        } else {
            ToolDecision::Allow
        };
        async move { Ok(decision) }
    }
}

/// The classification rule 3.2 predicts for one call, judging only by the records.
fn expected_code(records: &[JournalRecord], id: &str) -> Option<&'static str> {
    for record in records {
        if let JournalRecord::Message {
            message: Message::Tool { result, .. },
            ..
        } = record
            && result.call_id == id
        {
            return None; // already paired; repair leaves it alone
        }
    }
    if records.iter().any(|r| {
        matches!(r,
        JournalRecord::ToolExecutionSettled { call_id, .. } if call_id == id)
    }) {
        None // filled from the settled result, not from a code
    } else if records.iter().any(|r| {
        matches!(r,
        JournalRecord::ToolExecutionStarted { call_id, .. } if call_id == id)
    }) {
        Some("outcome_unknown_after_crash")
    } else {
        Some("not_started_after_crash")
    }
}

/// AC-A2-5: recovering at every possible crash point of one scripted batch yields a valid,
/// reopenable history, classifies every call per 3.2, and never executes a tool.
#[tokio::test(start_paused = true)]
async fn crash_traversal_recovers_every_prefix() {
    // A parallel shared pair, an exclusive call, a denied call and an unknown tool.
    let tools = MemTools::new(vec![
        (shared("a"), Behavior::Delay(5)),
        (spec("b"), Behavior::Echo),
        (spec("d"), Behavior::Echo),
    ]);
    let log = tools.log.clone();
    let model = ScriptedModel::new([
        calls(vec![
            call("s1", "a", json!({})),
            call("s2", "a", json!({})),
            call("e1", "b", json!({})),
            call("d1", "d", json!({})),
            call("u1", "missing", json!({})),
        ]),
        text("done"),
    ]);
    let journal = Arc::new(RecordingJournal::new());
    let mut chat = session_with_hooks(model, tools, journal.clone(), DenyD).await;
    chat.run_turn("go", Default::default(), &NoopSink)
        .await
        .unwrap();

    let records = journal.records();
    let all_ids = ["s1", "s2", "e1", "d1", "u1"];
    let executed_before = log.started().len();
    assert_eq!(
        executed_before, 3,
        "s1, s2 and e1 ran; d1 was denied, u1 unknown"
    );

    for k in 0..=records.len() {
        let prefix = &records[..k];
        let history = recover(prefix).unwrap();
        let replayed = replay(prefix).unwrap();

        // (1) Every call the prefix knows has exactly one result, in assistant order; a
        // prefix without the assistant message knows none.
        let known: Vec<String> = replayed
            .iter()
            .filter_map(|m| match m {
                Message::Assistant { tool_calls, .. } => {
                    Some(tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(result_ids(&history), known, "prefix {k}");

        // (2) Each fill matches the classification the prefix predicts.
        for id in all_ids {
            if !known.iter().any(|i| i == id) {
                continue;
            }
            let result = result_for(&history, id);
            match expected_code(prefix, id) {
                Some(expected) => assert_eq!(code(result), Some(expected), "prefix {k}, call {id}"),
                // Either the original result message, or the settled one verbatim.
                None => assert!(
                    code(result).is_none()
                        || matches!(code(result), Some(c) if c != "not_started_after_crash"
                            && c != "outcome_unknown_after_crash"),
                    "prefix {k}, call {id}: unexpected synthetic {result:?}"
                ),
            }
        }

        // (3) The recovered history validates ...
        Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
            .open_session_with_history(history)
            .await
            .unwrap();

        // (4) ... and a reopened session runs another turn.
        let fresh_tools = MemTools::new(vec![(spec("a"), Behavior::Echo)]);
        let fresh_model = ScriptedModel::new([text("fine")]);
        let mut reopened = Arc::new(Agent::new(fresh_model, fresh_tools.clone()))
            .open_session_with_history(replay(prefix).unwrap().clone())
            .await
            // The replayed prefix alone may have unpaired calls; with the repair it
            // always opens. Use the recovered history for the reopen instead.
            .map_err(|_| ())
            .ok();
        if let Some(session) = reopened.as_mut() {
            session
                .run_turn("next", Default::default(), &NoopSink)
                .await
                .unwrap();
        } else {
            let mut session = Arc::new(Agent::new(ScriptedModel::new([text("fine")]), fresh_tools))
                .open_session_with_history(recover(prefix).unwrap())
                .await
                .unwrap();
            session
                .run_turn("next", Default::default(), &NoopSink)
                .await
                .unwrap();
        }

        // (5) No tool executed anywhere in the traversal: the original counters stand.
        assert_eq!(log.started().len(), executed_before, "prefix {k}");
    }
}

/// A failed `Started` append ends the batch, but a call that is already running must still
/// hear the cancellation and get its grace period, not be dropped mid-flight.
#[tokio::test(start_paused = true)]
async fn a_failed_started_append_cancels_calls_already_running() {
    let tools = MemTools::new(vec![
        // Delayed, so both first calls are polled (really running) before c1 settles.
        (shared("quick"), Behavior::Delay(5)),
        (shared("clean"), Behavior::CleansUp(10)),
    ]);
    let log = tools.log.clone();
    let model = ScriptedModel::new([calls(vec![
        call("c1", "quick", json!({})),
        call("c2", "clean", json!({})),
        call("c3", "quick", json!({})),
    ])]);
    // Appends: 1 input, 2 assistant, 3-4 Started c1/c2, 5 Settled c1, 6 result c1,
    // 7 Started c3 (fails) while c2 is still running.
    let journal = Arc::new(FailingJournal::fail_at(7));
    let mut chat = session_with(model, tools, journal.clone()).await;
    let options = TurnOptions {
        max_parallel_tools: 2,
        ..Default::default()
    };

    let error = chat.run_turn("go", options, &NoopSink).await.unwrap_err();

    assert!(matches!(error, RunError::Journal(_)), "{error:?}");
    assert_eq!(log.started(), ["c1", "c2"]);
    assert_eq!(*log.cleaned_up.lock().unwrap(), ["c2"]);
    // No further appends were attempted after the failure, and c2 has no `Settled`.
    assert_eq!(journal.calls(), 7);
    assert!(!journal.records().iter().any(|r| matches!(r,
        JournalRecord::ToolExecutionSettled { call_id, .. } if call_id == "c2")));
}

/// `ToolStarted` is an observation of the durable marker: a failed `Started` append means
/// no event for that call.
#[tokio::test(start_paused = true)]
async fn tool_started_events_follow_the_durable_marker() {
    #[derive(Clone, Default)]
    struct Seen(Arc<std::sync::Mutex<Vec<String>>>);
    impl EventSink for Seen {
        fn emit(&self, event: AgentEvent) {
            if let AgentEvent::ToolStarted { call_id, .. } = event {
                self.0.lock().unwrap().push(call_id);
            }
        }
    }
    let model = ScriptedModel::new([calls(vec![call("c1", "a", json!({}))])]);
    let tools = MemTools::new(vec![(shared("a"), Behavior::Echo)]);
    let journal = Arc::new(FailingJournal::fail_at(3)); // 1 input, 2 assistant, 3 Started
    let mut chat = session_with(model, tools, journal).await;
    let seen = Seen::default();

    let error = chat
        .run_turn("go", Default::default(), &seen)
        .await
        .unwrap_err();

    assert!(matches!(error, RunError::Journal(_)), "{error:?}");
    assert!(seen.0.lock().unwrap().is_empty());
}
