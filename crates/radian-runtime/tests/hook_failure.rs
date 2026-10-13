//! A hook error ends the turn, not the session (ADR 0017), and the transcript stays valid.
//!
//! `before_model_call` and `after_model_response` are the only hook points that fail a turn;
//! both run before the assistant message of the step is committed. The tool-call hooks fail
//! closed instead. This drives the `before_model_call` case end to end: a turn fails after a
//! tool call, the next turn's request is still valid, and a resume sees the same history.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use agent_core::{
    Agent, AgentEvent, EventSink, HookContext, HookError, Hooks, InputSource, ModelRequest,
    RunStatus, TurnOptions,
};
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, input_json_delta, message_delta, message_start, message_stop,
    text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{MemTools, Recorder, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::json;

/// Fails `before_model_call` on the `fail_at`-th call (0-based), then allows every later one.
struct FailOnCall {
    calls: AtomicU32,
    fail_at: u32,
}

impl Hooks for FailOnCall {
    async fn before_model_call(
        &self,
        _cx: &HookContext,
        _request: &mut ModelRequest,
    ) -> Result<(), HookError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.fail_at {
            Err(HookError::new("the policy check failed"))
        } else {
            Ok(())
        }
    }
}

fn tool_reply(id: &str, name: &str) -> Reply {
    Reply::sse(&[
        message_start(None),
        tool_start(0, Some(id), name, json!({})),
        input_json_delta(0, "{}"),
        block_stop(0),
        message_delta("tool_use", None),
        message_stop(),
    ])
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

/// Waits for a turn to finish, with a bound: a mutation that makes the driver die instead of
/// continuing would otherwise hang the test binary.
async fn wait_turn(recorder: &Recorder) {
    tokio::time::timeout(std::time::Duration::from_secs(5), recorder.wait_finished())
        .await
        .expect("a turn must finish");
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

/// Every `tool_use` id and every `tool_result` id in one request body, so a test can assert
/// they match (a request with an unanswered call is invalid for the provider).
fn request_call_ids(body: &str) -> (Vec<String>, Vec<String>) {
    let value: serde_json::Value = serde_json::from_str(body).expect("the request body is JSON");
    let mut declared = Vec::new();
    let mut answered = Vec::new();
    for message in value["messages"].as_array().expect("messages") {
        for block in message["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("tool_use") => declared.push(block["id"].as_str().unwrap_or("").to_owned()),
                Some("tool_result") => {
                    answered.push(block["tool_use_id"].as_str().unwrap_or("").to_owned())
                }
                _ => {}
            }
        }
    }
    (declared, answered)
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
async fn a_before_model_hook_error_ends_the_turn_and_the_session_continues() {
    let server = MockServer::start(vec![
        // Turn 1, call 0: a tool call, so the transcript has an assistant tool call + result.
        tool_reply("toolu_1", "echo"),
        // Turn 2, call 1: the answer.
        text_reply("SECOND-OK"),
    ])
    .await;
    let dir = TempDir::new("hook-failure");
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let agent = Arc::new(
        Agent::new(
            Anthropic::new(
                AnthropicConfig::new(&server.base_url, "claude-test")
                    .with_api_key(ApiKey::new("sk-ant-hook")),
            )
            .unwrap(),
            MemTools::new(),
        )
        .with_hooks(FailOnCall {
            calls: AtomicU32::new(0),
            fail_at: 1,
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

    // Turn 1: the tool runs, then `before_model_call` fails on the second call.
    inbox.followup("first", InputSource::User).await.unwrap();
    wait_turn(&recorder).await;
    assert_eq!(last_status(&recorder), Some(RunStatus::Failed));

    // Turn 2: the hook allows it, and the model answers.
    inbox.followup("second", InputSource::User).await.unwrap();
    wait_turn(&recorder).await;
    assert_eq!(last_status(&recorder), Some(RunStatus::Completed));
    inbox.close();
    handle
        .await
        .unwrap()
        .expect("the driver survives the hook error");

    // The failure is durable and names the hook point.
    let messages = turn_failed_messages(&path);
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("BeforeModelCall"), "{}", messages[0]);
    assert!(
        messages[0].contains("policy check failed"),
        "{}",
        messages[0]
    );

    // Turn 2's request is valid: every declared call has a result.
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "turn 1's second call was never sent");
    let (declared, answered) = request_call_ids(&requests[1].body);
    assert!(!declared.is_empty(), "{}", requests[1].body);
    assert_eq!(declared, answered, "{}", requests[1].body);

    // History: both turns once each, and the answer; the tool call kept its result.
    let history: Vec<String> = Session::open(&path)
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .map(|message| message.text())
        .collect();
    assert_eq!(
        history.iter().filter(|t| *t == "first").count(),
        1,
        "{history:?}"
    );
    assert_eq!(
        history.iter().filter(|t| *t == "second").count(),
        1,
        "{history:?}"
    );
    assert!(history.contains(&"SECOND-OK".to_string()), "{history:?}");
}
