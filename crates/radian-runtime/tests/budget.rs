//! Context budget end to end (ADR 0015 projection, ADR 0018): a turn whose projection is over
//! the input budget `B` still sends its requests, completes, and writes **no**
//! `ContextBudgetReached` event. The projection is kept; the stop is gone.

mod common;

use std::path::Path;
use std::sync::Arc;

use agent_core::{
    Agent, AgentConfig, ContextBudget, InputSource, JournalRecord, Message, ModelResponse,
    ModelUsage, TranscriptJournal, TurnOptions, UsageSource,
};
use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, RuntimeEvent, Session, SessionJournal, TurnOutcome};
use serde_json::json;

struct Env {
    inbox: Arc<Inbox>,
    /// Held so the temp directory (and the session file inside it) outlives the driver.
    #[allow(dead_code)]
    dir: TempDir,
    path: std::path::PathBuf,
}

async fn open_env(
    label: &str,
    model: Arc<ScriptedModel>,
    tools: MemTools,
    budget: Option<ContextBudget>,
) -> (Driver<Arc<ScriptedModel>, MemTools, ()>, Env) {
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(
        Agent::new(Arc::clone(&model), tools).with_config(AgentConfig {
            context_budget: budget,
            ..Default::default()
        }),
    );
    let driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (driver, Env { inbox, dir, path })
}

fn used(response: ModelResponse, input: u64) -> ModelResponse {
    ModelResponse {
        usage: Some(ModelUsage {
            input: Some(input),
            ..Default::default()
        }),
        ..response
    }
}

/// Waits (without sleeping) until `n` turns have durably finished, whatever their outcome.
async fn wait_for_turns(path: &Path, n: usize) {
    for _ in 0..1_000_000 {
        let count = std::fs::read_to_string(path)
            .map(|text| text.matches("\"TurnFinished\"").count())
            .unwrap_or(0);
        if count >= n {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("fewer than {n} TurnFinished events landed in {path:?}");
}

fn assert_no_context_budget_event(path: &Path) -> Session {
    let session = Session::open(path).unwrap();
    assert!(
        session
            .events()
            .iter()
            .all(|event| !matches!(&event.data, EventData::ContextBudgetReached { .. })),
        "ADR 0018 removed the stop: no ContextBudgetReached may be written"
    );
    session
}

/// 1. A reported usage over `B`: both requests are sent, the turn completes, and no
///    `ContextBudgetReached` is written (ADR 0018).
#[tokio::test]
async fn an_over_budget_turn_completes_and_writes_no_context_budget_event() {
    let model = ScriptedModel::new([
        used(calls(vec![call("c1", "echo", json!({"a": 1}))]), 900),
        used(text("second reply"), 950),
    ]);
    let (mut driver, env) = open_env(
        "over-budget",
        Arc::clone(&model),
        MemTools::new(),
        Some(ContextBudget::new(800)),
    )
    .await;
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_turns(&env.path, 1).await;
    handle.abort();
    let _ = handle.await;

    // The over-budget request was sent: two model calls.
    assert_eq!(model.requests().len(), 2);
    let session = assert_no_context_budget_event(&env.path);
    assert!(session.events().iter().any(|event| matches!(
        &event.data,
        EventData::TurnFinished {
            outcome: TurnOutcome::Completed,
            ..
        }
    )));
}

/// 2. A real 64 KiB tool result pushes the projection over `B`; the next request is still sent
///    and the turn completes.
#[tokio::test]
async fn a_large_tool_result_does_not_stop_the_next_request_end_to_end() {
    let big = json!({"blob": "x".repeat(64 * 1024)});
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", big)]), text("done")]);
    let (mut driver, env) = open_env(
        "over-budget-e2e",
        Arc::clone(&model),
        MemTools::new(),
        Some(ContextBudget::new(1000)),
    )
    .await;
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_turns(&env.path, 1).await;
    handle.abort();
    let _ = handle.await;

    assert_eq!(model.requests().len(), 2);
    let session = assert_no_context_budget_event(&env.path);
    assert!(session.events().iter().any(|event| matches!(
        &event.data,
        EventData::TurnFinished {
            outcome: TurnOutcome::Completed,
            ..
        }
    )));
}

/// Resumes a driver over an existing session file (the previous driver's journal handle must
/// already be dropped, so the single-writer lock is free).
#[allow(clippy::type_complexity)]
async fn resume_driver(
    path: &Path,
    model: Arc<ScriptedModel>,
    budget: Option<ContextBudget>,
) -> (
    Driver<Arc<ScriptedModel>, MemTools, ()>,
    Arc<Inbox>,
    Arc<Recorder>,
) {
    let journal = Arc::new(SessionJournal::resume(path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(Arc::clone(&model), MemTools::new()).with_config(
        AgentConfig {
            context_budget: budget,
            ..Default::default()
        },
    ));
    let driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();
    (driver, inbox, recorder)
}

/// 3. A session whose turn was over `B` resumes normally: the next turn runs and the history
///    stays consistent. (The restored baseline is covered by `agent-core`'s
///    `a_restored_baseline_matches_an_uninterrupted_projection`.)
#[tokio::test]
async fn resume_after_an_over_budget_turn_keeps_the_history() {
    let model = ScriptedModel::new([used(text("first"), 900)]);
    let (mut driver, env) = open_env(
        "resume-over-budget",
        Arc::clone(&model),
        MemTools::new(),
        Some(ContextBudget::new(800)),
    )
    .await;
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_turns(&env.path, 1).await;
    handle.abort();
    let _ = handle.await;
    let path = env.path.clone();
    drop(env.inbox);

    let resumed_model = Arc::new(ScriptedModel::new([text("second")]));
    let (mut driver2, inbox2, recorder2) = resume_driver(
        &path,
        Arc::clone(&resumed_model),
        Some(ContextBudget::new(800)),
    )
    .await;
    let handle2 = tokio::spawn(async move {
        let _ = driver2.run().await;
    });
    inbox2.followup("more", InputSource::User).await.unwrap();
    let _ = recorder2.wait_finished().await;
    handle2.abort();
    let _ = handle2.await;

    // The resumed turn ran (its request went out) and completed.
    assert_eq!(resumed_model.requests().len(), 1);
    assert_no_context_budget_event(&path);
    let history: Vec<String> = Session::open(&path)
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .map(|message| message.text())
        .collect();
    for expected in ["go", "first", "more", "second"] {
        assert!(
            history.iter().any(|text| text == expected),
            "missing {expected:?} in {history:?}"
        );
    }
}

/// 4. A session file written **before** ADR 0018 still carries a `context_budget` outcome and
///    a `ContextBudgetReached` event. The reader must keep accepting both (ADR 0006 §7(d):
///    historical values stay readable), and a driver must still resume the session.
#[tokio::test]
async fn a_historical_budget_stop_still_reads_and_resumes() {
    let dir = TempDir::new("historical-stop");
    let path = dir.file("s.jsonl");
    {
        let journal = SessionJournal::create(&path, "sid", created()).unwrap();
        for (index, message) in [(0usize, Message::user("go")), (1, Message::assistant("stop"))] {
            journal
                .append(JournalRecord::Message { index, message })
                .await
                .unwrap();
        }
        for event in [
            RuntimeEvent::TurnFinished {
                turn_id: "t-1".into(),
                outcome: TurnOutcome::ContextBudget,
                reason: None,
                stop_reason: None,
            },
            RuntimeEvent::ContextBudgetReached {
                turn_id: "t-1".into(),
                source: UsageSource::Estimated,
                projected_tokens: 4200,
                window_tokens: 4000,
                threshold_percent: 80,
                baseline_input_tokens: None,
                estimated_growth_tokens: None,
            },
        ] {
            journal.append_runtime_event(event).await.unwrap();
        }
    } // the writer lock is released here

    // The reader still parses the historical outcome and event.
    let session = Session::open(&path).unwrap();
    assert!(session.events().iter().any(|event| matches!(
        &event.data,
        EventData::TurnFinished {
            outcome: TurnOutcome::ContextBudget,
            ..
        }
    )));
    assert!(session.events().iter().any(|event| matches!(
        &event.data,
        EventData::ContextBudgetReached { .. }
    )));

    // And a driver still resumes it and runs a new turn.
    let model = Arc::new(ScriptedModel::new([text("after resume")]));
    let (mut driver, inbox, recorder) = resume_driver(&path, Arc::clone(&model), None).await;
    let handle = tokio::spawn(async move {
        let _ = driver.run().await;
    });
    inbox.followup("more", InputSource::User).await.unwrap();
    let _ = recorder.wait_finished().await;
    handle.abort();
    let _ = handle.await;

    assert_eq!(model.requests().len(), 1, "the resumed turn must send a request");
}
