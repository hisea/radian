//! Extended-thinking round trip across a real close and resume (ADR 0014 R10; wire notes
//! §2.3/§4.5).
//!
//! One turn streams a `thinking` block (with its signature) followed by a `tool_use`; the
//! tool runs and a second response ends the turn. The session is closed, reopened from its
//! own JSONL file, and a new turn is sent. The request that turn makes must carry the
//! assistant message's `thinking` block — signature included — back to Anthropic unchanged.
//! The JSONL file is the only memory between the two drivers.

mod common;

use std::sync::Arc;

use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, input_json_delta, message_delta, message_start, message_stop,
    text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey, ThinkingConfig};
use common::{MemTools, Recorder, TempDir, created};
use radian_core::{Agent, EventSink, InputSource, TurnOptions};
use radian_runtime::{Driver, Inbox};
use radian_session::SessionJournal;
use serde_json::json;

const SESSION_ID: &str = "think";

fn model(server: &MockServer) -> Anthropic {
    let config = AnthropicConfig::new(&server.base_url, "claude-test")
        .with_api_key(ApiKey::new("sk-ant-thinking-test"))
        .with_thinking(ThinkingConfig::Adaptive);
    Anthropic::new(config).expect("a local mock config is valid")
}

/// A thinking block with a signature, then a `tool_use` (the shape wire notes §4.5 requires
/// to be echoed back complete).
fn thinking_tool_reply(thinking: &str, signature: &str) -> Reply {
    Reply::sse(&[
        message_start(None),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "thinking_delta", "thinking": thinking}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "signature_delta", "signature": signature}}),
        block_stop(0),
        tool_start(1, Some("toolu_1"), "echo", json!({})),
        input_json_delta(1, r#"{"a":1}"#),
        block_stop(1),
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

/// Opens a driver over `journal` and runs exactly one turn, then closes it.
async fn one_turn(journal: Arc<SessionJournal>, server: &MockServer, input: &str) {
    let agent = Arc::new(Agent::new(model(server), MemTools::new()));
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let mut driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn EventSink>,
        TurnOptions::default(),
    )
    .await
    .expect("open the driver");
    let handle = tokio::spawn(async move { driver.run().await });
    inbox.followup(input, InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    inbox.close();
    handle.await.unwrap().expect("the driver ends cleanly");
}

fn assistant_payload(request: &serde_json::Value) -> Option<serde_json::Value> {
    request["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "assistant")
        .map(|message| message["content"].clone())
}

#[tokio::test]
async fn a_thinking_block_survives_close_and_resume() {
    let server = MockServer::start(vec![
        thinking_tool_reply("I should call the tool", "SIG-abc123"),
        text_reply("DONE"),
        text_reply("AFTER-RESUME"),
    ])
    .await;
    let dir = TempDir::new("thinking-resume");
    let path = dir.file("think.jsonl");

    // Turn 1: thinking + tool_use, the tool runs, the second request ends the turn.
    let journal = Arc::new(SessionJournal::create(&path, SESSION_ID, created()).unwrap());
    one_turn(Arc::clone(&journal), &server, "use the tool").await;
    drop(journal);

    // Resume from the file alone.
    let resumed = Arc::new(SessionJournal::resume(&path, SESSION_ID).unwrap());
    one_turn(resumed, &server, "continue").await;

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "two requests in turn 1, one after resume"
    );
    // The resumed request (the third) carries the whole first response's conversation ...
    let body = requests[2].json();
    let content = assistant_payload(&body).expect("an assistant message in the resumed request");
    let blocks = content
        .as_array()
        .expect("assistant content is a block array");
    // ... including the thinking block with its signature, ahead of the tool_use.
    assert_eq!(
        blocks.first(),
        Some(&json!({
            "type": "thinking",
            "thinking": "I should call the tool",
            "signature": "SIG-abc123"
        })),
        "the thinking block must be echoed back unchanged: {blocks:?}"
    );
    assert_eq!(
        blocks.get(1).map(|b| b["type"].clone()),
        Some(json!("tool_use"))
    );
    // The tool result is still there, associated with the call.
    assert!(
        body.to_string().contains("toolu_1"),
        "the tool_use must be echoed too: {body}"
    );
}
