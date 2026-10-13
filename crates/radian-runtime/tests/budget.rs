//! Context budget end to end: the stop is durable and readable, and a real 64 KiB tool
//! result is what pushes the next request over the line (ADR 0015).

mod common;

use std::path::Path;
use std::sync::Arc;

use agent_core::{
    Agent, AgentConfig, ContextBudget, InputSource, ModelResponse, ModelUsage, TurnOptions,
    UsageSource,
};
use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal, TurnOutcome};
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

/// Waits (without sleeping) until the turn's durable `context_budget` outcome is in the file.
async fn wait_for_turn_finished(path: &Path) {
    for _ in 0..500_000 {
        if std::fs::read_to_string(path)
            .map(|text| text.contains("\"context_budget\""))
            .unwrap_or(false)
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("no context_budget TurnFinished landed in {path:?}");
}

/// 6. The stop is written to the session file, reads back, and its reason is distinguishable.
#[tokio::test]
async fn the_stop_is_journalled_and_read_back() {
    let model = ScriptedModel::new([used(calls(vec![call("c1", "echo", json!({"a": 1}))]), 900)]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env(
        "budget-stop",
        Arc::clone(&model),
        tools,
        Some(ContextBudget::new(1000)),
    )
    .await;
    let handle = tokio::spawn(async move { driver.run().await });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_turn_finished(&env.path).await;
    handle.abort();

    let session = Session::open(&env.path).unwrap();
    let events = session.events();
    let reached = events
        .iter()
        .find_map(|event| match &event.data {
            EventData::ContextBudgetReached {
                source,
                projected_tokens,
                window_tokens,
                threshold_percent,
                baseline_input_tokens,
                estimated_growth_tokens,
                ..
            } => Some((
                *source,
                *projected_tokens,
                *window_tokens,
                *threshold_percent,
                *baseline_input_tokens,
                *estimated_growth_tokens,
            )),
            _ => None,
        })
        .expect("ContextBudgetReached is durable");
    assert_eq!(reached.0, UsageSource::ReportedPlusEstimate);
    assert!(
        reached.1 >= 800,
        "projection {} is past the 80% line",
        reached.1
    );
    assert_eq!(reached.2, 1000);
    assert_eq!(reached.3, 80);
    assert_eq!(reached.4, Some(900));
    assert!(reached.5.unwrap() > 0, "the growth is the estimated part");
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::TurnFinished {
            outcome: TurnOutcome::ContextBudget,
            ..
        }
    )));
    // The second (over-budget) request never reached the model.
    assert_eq!(model.requests().len(), 1);
}

/// 7. A 64 KiB tool result pushes the projection over the line, so the next request is
///    blocked — the reason this slice exists, proven through the driver.
#[tokio::test]
async fn a_large_tool_result_blocks_the_next_request_end_to_end() {
    let big = json!({"blob": "x".repeat(64 * 1024)});
    let model = ScriptedModel::new([calls(vec![call("c1", "echo", big)]), text("never sent")]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env(
        "budget-e2e",
        Arc::clone(&model),
        tools,
        Some(ContextBudget::new(1000)),
    )
    .await;
    let handle = tokio::spawn(async move { driver.run().await });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_turn_finished(&env.path).await;
    handle.abort();

    assert_eq!(model.requests().len(), 1);
    let session = Session::open(&env.path).unwrap();
    let events = session.events();
    let source = events.iter().find_map(|event| match &event.data {
        EventData::ContextBudgetReached { source, .. } => Some(*source),
        _ => None,
    });
    // No reply carried usage, so the projection is honestly labelled an estimate.
    assert_eq!(source, Some(UsageSource::Estimated));
    assert!(events.iter().any(|event| matches!(
        &event.data,
        EventData::TurnFinished {
            outcome: TurnOutcome::ContextBudget,
            ..
        }
    )));
}

/// Waits (without sleeping) until `n` turns have durably ended as `context_budget`.
async fn wait_for_budget_turns(path: &Path, n: usize) {
    for _ in 0..1_000_000 {
        let count = std::fs::read_to_string(path)
            .map(|text| text.matches("\"context_budget\"").count())
            .unwrap_or(0);
        if count >= n {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("fewer than {n} context_budget turns landed in {path:?}");
}

/// 8. The host's "continue" relaxes the next turn's first request; the request after it is
///    blocked again, so the relax is once and never a permanent switch (ADR 0015 C5).
#[tokio::test]
async fn relaxing_once_sends_the_blocked_request_and_restores_the_guard() {
    let big = json!({"blob": "x".repeat(64 * 1024)});
    let model = ScriptedModel::new([
        used(calls(vec![call("c1", "echo", big.clone())]), 900),
        calls(vec![call("c2", "echo", big)]),
        text("never sent"),
    ]);
    let tools = MemTools::new();
    let (mut driver, env) = open_env(
        "budget-relax",
        Arc::clone(&model),
        tools,
        Some(ContextBudget::new(1000)),
    )
    .await;
    // The handle is taken before the driver moves onto its task; it is a clone of the flag.
    let relax = driver.budget_relax();
    let handle = tokio::spawn(async move { driver.run().await });
    env.inbox.followup("go", InputSource::User).await.unwrap();
    wait_for_budget_turns(&env.path, 1).await;
    assert_eq!(
        model.requests().len(),
        1,
        "the over-budget request must not have been sent"
    );

    // The host saw the stop and chose continue: the next turn's first request is let through.
    relax.relax_once();
    env.inbox
        .followup("continue", InputSource::User)
        .await
        .unwrap();
    wait_for_budget_turns(&env.path, 2).await;
    assert_eq!(
        model.requests().len(),
        2,
        "the relaxed request was sent, and the one after it was blocked again"
    );

    // Turn 3: no new `relax_once`. The flag was consumed by turn 2, so the first request is
    // blocked again; a `take` that left the flag set (`load` instead of `swap`) would let it
    // through and send a third request.
    env.inbox.followup("more", InputSource::User).await.unwrap();
    wait_for_turns(&env.path, 3).await;
    assert_eq!(
        model.requests().len(),
        2,
        "the one-shot relax must not leak into a later turn"
    );
    handle.abort();
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
