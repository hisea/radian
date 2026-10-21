//! What a failed provider request does to the turn and the session (ADR 0017).
//!
//! A model error ends **this turn** with `TurnFinished { outcome: failed }` and a sibling
//! `TurnFailed { message }`; the driver keeps running, so the next input opens a fresh turn.
//! Every other `RunError` (journal, hook, session, poisoned) is still fatal.

mod common;

use std::path::Path;
use std::sync::Arc;

use radian_core::{Agent, AgentEvent, EventSink, InputSource, NoTools, RunStatus, TurnOptions};
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, message_delta, message_start, message_stop, text_delta,
    text_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{Recorder, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};

const SECRET: &str = "sk-ant-failure-test";

fn model(server: &MockServer) -> Anthropic {
    let config =
        AnthropicConfig::new(&server.base_url, "claude-test").with_api_key(ApiKey::new(SECRET));
    Anthropic::new(config).expect("a local mock config is valid")
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, text),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])
}

fn last_status(recorder: &Recorder) -> Option<RunStatus> {
    recorder
        .events()
        .into_iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::RunFinished { status } => Some(status),
            _ => None,
        })
}

/// Runs `inputs` one turn at a time over one driver, returning each turn's `RunStatus` and
/// the driver's final result. The driver is closed after the last turn.
async fn run_inputs(
    path: &Path,
    server: &MockServer,
    inputs: &[&str],
) -> (Vec<RunStatus>, Result<(), String>) {
    let journal = Arc::new(SessionJournal::create(path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(
        Agent::new(model(server), NoTools).with_config(radian_core::AgentConfig {
            // These tests pin the one-failure-per-turn behaviour (ADR 0017 §2); its
            // retry is exercised in radian-core's retry tests.
            retries: 0,
            ..Default::default()
        }),
    );
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

    let mut statuses = Vec::new();
    for input in inputs {
        inbox.followup(*input, InputSource::User).await.unwrap();
        recorder.wait_finished().await;
        statuses.push(last_status(&recorder).expect("a turn finished"));
    }
    inbox.close();
    let result = handle
        .await
        .expect("the driver task does not panic")
        .map_err(|e| e.to_string());
    (statuses, result)
}

fn turn_finished(path: &Path) -> Vec<(String, String)> {
    Session::open(path)
        .unwrap()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TurnFinished { outcome, .. } => {
                Some((event.event.clone(), format!("{outcome:?}")))
            }
            _ => None,
        })
        .collect()
}

fn turn_failed_messages(path: &Path) -> Vec<String> {
    Session::open(path)
        .unwrap()
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TurnFailed { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_failed_model_request_ends_the_turn_and_writes_turn_finished() {
    let server = MockServer::start(vec![Reply::status(500, "provider is down")]).await;
    let dir = TempDir::new("provider-failure");
    let path = dir.file("s.jsonl");

    let (statuses, driver_result) = run_inputs(&path, &server, &["hi"]).await;

    assert_eq!(statuses, vec![RunStatus::Failed]);
    // The driver did **not** end with the error: the session is still usable.
    assert_eq!(
        driver_result,
        Ok(()),
        "the driver must survive a model error"
    );

    let session = Session::open(&path).unwrap();
    let history = session.history().unwrap();
    assert_eq!(history.len(), 1, "only the user message: {history:?}");
    assert_eq!(history[0].text(), "hi");

    // The durable terminal record names the failure ...
    assert_eq!(
        turn_finished(&path),
        vec![("TurnFinished".into(), "Failed".into())]
    );
    // ... and the sibling `TurnFailed` carries the (masked) message.
    let messages = turn_failed_messages(&path);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("HTTP 500"), "{}", messages[0]);
    assert!(messages[0].contains("provider is down"), "{}", messages[0]);
}

#[tokio::test]
async fn a_failed_turn_does_not_end_the_session() {
    let server = MockServer::start(vec![
        Reply::status(500, "first fails"),
        text_reply("SECOND-OK"),
    ])
    .await;
    let dir = TempDir::new("provider-recover");
    let path = dir.file("s.jsonl");

    let (statuses, driver_result) = run_inputs(&path, &server, &["first", "second"]).await;

    assert_eq!(statuses, vec![RunStatus::Failed, RunStatus::Completed]);
    assert_eq!(driver_result, Ok(()));
    let history = Session::open(&path).unwrap().history().unwrap();
    let texts: Vec<String> = history.iter().map(|m| m.text()).collect();
    assert_eq!(texts, vec!["first", "second", "SECOND-OK"]);
    assert_eq!(turn_finished(&path).len(), 2);
}

#[tokio::test]
async fn a_cut_stream_ends_the_turn_and_leaves_the_same_consistent_history() {
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: vec![
            format!(
                "data: {}\n\n",
                serde_json::json!({"type": "message_start", "message": {"usage": {}}})
            )
            .into_bytes(),
            format!(
                "data: {}\n\n",
                serde_json::json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}})
            )
            .into_bytes(),
            format!(
                "data: {}\n\n",
                serde_json::json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "half"}})
            )
            .into_bytes(),
        ],
    }])
    .await;
    let dir = TempDir::new("provider-cut");
    let path = dir.file("s.jsonl");

    let (statuses, driver_result) = run_inputs(&path, &server, &["hi"]).await;
    assert_eq!(statuses, vec![RunStatus::Failed]);
    assert_eq!(driver_result, Ok(()));

    let history = Session::open(&path).unwrap().history().unwrap();
    assert_eq!(
        history.len(),
        1,
        "the partial answer is not kept: {history:?}"
    );
    assert_eq!(history[0].text(), "hi");
    assert_eq!(turn_finished(&path).len(), 1);
}

#[tokio::test]
async fn resume_after_a_failed_turn_keeps_the_history() {
    let server =
        MockServer::start(vec![Reply::status(500, "down"), text_reply("RESUMED-OK")]).await;
    let dir = TempDir::new("provider-resume");
    let path = dir.file("s.jsonl");

    let (statuses, _) = run_inputs(&path, &server, &["first"]).await;
    assert_eq!(statuses, vec![RunStatus::Failed]);

    // Resume from the file alone, then run a successful turn.
    let journal = Arc::new(SessionJournal::resume(&path, "sid").unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(Agent::new(model(&server), NoTools).with_config(
        radian_core::AgentConfig {
            retries: 0,
            ..Default::default()
        },
    ));
    let mut driver = Driver::open(
        &agent,
        journal,
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .expect("resume the driver");
    let handle = tokio::spawn(async move { driver.run().await });
    inbox.followup("second", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    assert_eq!(last_status(&recorder), Some(RunStatus::Completed));
    inbox.close();
    handle.await.unwrap().unwrap();

    let history = Session::open(&path).unwrap().history().unwrap();
    let texts: Vec<String> = history.iter().map(|m| m.text()).collect();
    assert_eq!(texts, vec!["first", "second", "RESUMED-OK"]);
}

/// The `TurnFailed` message is durable, so the key must never reach the session file (the
/// adapter masks it first; this pins the durable record, not just the live text).
#[tokio::test]
async fn the_turn_failed_message_never_contains_the_key() {
    let server = MockServer::start(vec![Reply::status(500, &format!("bad key {SECRET}"))]).await;
    let dir = TempDir::new("provider-secret");
    let path = dir.file("s.jsonl");

    let (statuses, _) = run_inputs(&path, &server, &["hi"]).await;
    assert_eq!(statuses, vec![RunStatus::Failed]);
    let file = std::fs::read_to_string(&path).unwrap();
    assert!(file.contains("<redacted>"), "{file}");
    assert!(
        !file.contains(SECRET),
        "the key leaked into the session file"
    );
}
