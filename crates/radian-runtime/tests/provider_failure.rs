//! What a failed provider request leaves in the session file (Phase 7).
//!
//! A model error ends the turn with `RunError::Model`, which the driver propagates: the
//! driver stops and the host reports the error. This records what that does to the durable
//! session — the summary is in `docs/provider-contract.md`.
//!
//! `a_failed_turn_should_still_write_a_turn_finished` is `#[ignore]`d on purpose: it asserts
//! the *desired* behavior and fails against today's code (no `TurnFinished` is written when a
//! turn fails). It is the stable reproduction of that gap.

mod common;

use std::path::Path;
use std::sync::Arc;

use agent_core::{Agent, EventSink, InputSource, NoTools, TurnOptions};
use agent_provider_anthropic::testing::{MockServer, Reply};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{Recorder, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};

fn model(server: &MockServer) -> Anthropic {
    let config = AnthropicConfig::new(&server.base_url, "claude-test")
        .with_api_key(ApiKey::new("sk-ant-failure-test"));
    Anthropic::new(config).expect("a local mock config is valid")
}

/// Runs one turn that must fail, and returns the driver's error message.
async fn run_failed_turn(path: &Path, server: &MockServer) -> Option<String> {
    let journal = Arc::new(SessionJournal::create(path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(model(server), NoTools));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .expect("open the driver");
    let handle = tokio::spawn(async move { driver.run().await });
    inbox.followup("hi", InputSource::User).await.unwrap();
    handle
        .await
        .expect("the driver task does not panic")
        .err()
        .map(|e| e.to_string())
}

fn count_finished(path: &Path) -> usize {
    Session::open(path)
        .unwrap()
        .events()
        .iter()
        .filter(|event| matches!(event.data, EventData::TurnFinished { .. }))
        .count()
}

#[tokio::test]
async fn a_failed_model_request_leaves_the_user_turn_and_no_assistant_message() {
    let server = MockServer::start(vec![Reply::status(500, "provider is down")]).await;
    let dir = TempDir::new("provider-failure");
    let path = dir.file("s.jsonl");

    let error = run_failed_turn(&path, &server).await;
    assert!(
        error.as_deref().is_some_and(|e| e.contains("HTTP 500")),
        "the driver must end with the model error: {error:?}"
    );

    // The user turn is durable and admitted; the failed response wrote nothing.
    let session = Session::open(&path).unwrap();
    let history = session.history().unwrap();
    assert_eq!(history.len(), 1, "only the user message: {history:?}");
    assert_eq!(history[0].text(), "hi");
    assert!(
        session
            .events()
            .iter()
            .any(|event| matches!(event.data, EventData::TurnStarted { .. })),
        "the turn started"
    );
    // Current behavior: a failed turn writes no `TurnFinished`.
    assert_eq!(
        count_finished(&path),
        0,
        "today a failed turn leaves no durable terminal record"
    );
}

#[tokio::test]
async fn a_cut_stream_leaves_the_same_consistent_history() {
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: vec![
            format!("data: {}\n\n", serde_json::json!({"type": "message_start", "message": {"usage": {}}})).into_bytes(),
            format!("data: {}\n\n", serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})).into_bytes(),
            format!("data: {}\n\n", serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "half"}})).into_bytes(),
        ],
    }])
    .await;
    let dir = TempDir::new("provider-cut");
    let path = dir.file("s.jsonl");

    let error = run_failed_turn(&path, &server).await;
    assert!(error.is_some(), "the driver must end with the error");

    let session = Session::open(&path).unwrap();
    let history = session.history().unwrap();
    assert_eq!(
        history.len(),
        1,
        "the partial answer is not kept: {history:?}"
    );
    assert_eq!(history[0].text(), "hi");
    assert_eq!(count_finished(&path), 0);
}

/// The gap this slice records: a failed turn should still leave a durable `TurnFinished`
/// (with `outcome: "failed"`), so a reader can tell the turn ended rather than that the
/// process died. Fails against today's code; run with `--ignored`.
#[tokio::test]
#[ignore = "known gap: a failed turn writes no TurnFinished (docs/provider-contract.md)"]
async fn a_failed_turn_should_still_write_a_turn_finished() {
    let server = MockServer::start(vec![Reply::status(500, "provider is down")]).await;
    let dir = TempDir::new("provider-failure-gap");
    let path = dir.file("s.jsonl");
    let _ = run_failed_turn(&path, &server).await;
    assert!(
        count_finished(&path) >= 1,
        "a failed turn should have a durable terminal record"
    );
}
