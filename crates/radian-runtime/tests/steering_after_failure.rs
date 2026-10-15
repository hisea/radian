//! A steer accepted during a turn that then ends without applying it must not be lost
//! (ADR 0011, ADR 0017).
//!
//! `drain_steering` hands a `next_step` input to Core as soon as it can; Core only applies it
//! at the next safe point. On an `end_turn` finish Core keeps the turn going when steering is
//! pending, but a model error — and any non-`end_turn` finish such as `max_tokens` — ends the
//! turn with the steer still unapplied. The entry was claimed but never admitted; the driver
//! puts it back at the front of its lane so the next turn uses it exactly once.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_core::{Agent, AgentEvent, EventSink, InputSource, RunStatus, TurnOptions};
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, input_json_delta, message_delta, message_start, message_stop,
    text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{MemTools, Recorder, TempDir, created};
use radian_runtime::{Driver, Inbox};
use radian_session::{Session, SessionJournal};
use serde_json::json;

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

/// A final answer whose stop reason is `max_tokens`, sent slowly so a steer can land during
/// the call. Core does **not** keep the turn going for `max_tokens`, so the steer is left
/// unapplied.
fn slow_max_tokens_reply(text: &str) -> Reply {
    Reply::Sse {
        pieces: [
            message_start(None),
            text_start(0),
            text_delta(0, text),
            block_stop(0),
            message_delta("max_tokens", None),
            message_stop(),
        ]
        .iter()
        .map(|event| format!("data: {event}\n\n").into_bytes())
        .collect(),
        pause: Duration::from_millis(400),
    }
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

/// Opens a driver over a real session and runs the scenario: turn 1 calls the `gate` tool,
/// the steer is injected while the second model call is in flight, then turn 2 runs. Returns
/// the request bodies and the session path.
async fn run_scenario(
    label: &str,
    second_reply: Reply,
    idle_timeout: Option<Duration>,
) -> (Vec<String>, Vec<String>, RunStatus) {
    let server = MockServer::start(vec![
        tool_reply("toolu_1", "gate"),
        second_reply,
        text_reply("SECOND-OK"),
    ])
    .await;
    let dir = TempDir::new(label);
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, "sid", created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let tools = MemTools::new();
    let started = Arc::clone(&tools.started);
    let gate = Arc::clone(&tools.gate);

    let mut config = AnthropicConfig::new(&server.base_url, "claude-test")
        .with_api_key(ApiKey::new("sk-ant-steer"));
    if let Some(timeout) = idle_timeout {
        config.idle_timeout = timeout;
    }
    let agent = Arc::new(
        Agent::new(Anthropic::new(config).unwrap(), tools).with_config(agent_core::AgentConfig {
            // The scenario drives a failing turn (a transient idle timeout); pin the
            // pre-revision one-shot behaviour so the retry does not consume the next scripted
            // reply (ADR 0017's retry is covered in agent-core's `retry` tests).
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

    // Turn 1: run the gate tool, release it, then inject the steer while the second call is
    // in flight (after the last `inject_steering` of the turn).
    inbox.followup("go", InputSource::User).await.unwrap();
    started.notified().await;
    gate.notify_one();
    while server.requests().len() < 2 {
        tokio::task::yield_now().await;
    }
    inbox.steer("STEER-ME", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    let first_status = last_status(&recorder).expect("a turn finished");

    // Turn 2: the requeued steer joins the next turn and the model answers.
    inbox.followup("second", InputSource::User).await.unwrap();
    recorder.wait_finished().await;
    assert_eq!(last_status(&recorder), Some(RunStatus::Completed));
    inbox.close();
    handle.await.unwrap().expect("the driver survives");

    let requests: Vec<String> = server.requests().into_iter().map(|r| r.body).collect();
    assert_eq!(requests.len(), 3, "three model calls expected");
    assert_eq!(
        requests[2].matches("STEER-ME").count(),
        1,
        "the steer must appear exactly once in the next request: {}",
        requests[2]
    );
    let history: Vec<String> = Session::open(&path)
        .unwrap()
        .history()
        .unwrap()
        .iter()
        .map(|message| message.text())
        .collect();
    assert_eq!(
        history.iter().filter(|text| *text == "STEER-ME").count(),
        1,
        "the steer must be in the history exactly once: {history:?}"
    );
    (requests, history, first_status)
}

/// A model error (ADR 0017) ends the turn with the steer unapplied.
#[tokio::test]
async fn a_steer_accepted_during_a_failing_turn_is_requeued_and_used_once() {
    // A short idle timeout gives the test a window to inject the steer into the failing call.
    let (requests, history, first_status) = run_scenario(
        "steer-failure",
        Reply::SseThenHang { pieces: Vec::new() },
        Some(Duration::from_secs(2)),
    )
    .await;
    assert_eq!(first_status, RunStatus::Failed);
    // The first two calls were turn 1's; the third (turn 2) carried the steer.
    assert!(requests[0].contains("go"), "{}", requests[0]);
    assert!(requests[1].contains("tool_result"), "{}", requests[1]);
    assert!(history.contains(&"SECOND-OK".to_string()), "{history:?}");
}

/// The same gap existed on a non-`end_turn` finish: a `max_tokens` answer ends the turn even
/// with steering pending, so the steer was dropped in-process. The requeue covers it too.
#[tokio::test]
async fn a_steer_accepted_during_a_max_tokens_finish_is_requeued_and_used_once() {
    let (_requests, history, first_status) =
        run_scenario("steer-max-tokens", slow_max_tokens_reply("FINAL"), None).await;
    assert!(
        matches!(first_status, RunStatus::Incomplete(_)),
        "turn 1 must end incomplete: {first_status:?}"
    );
    assert!(history.contains(&"SECOND-OK".to_string()), "{history:?}");
}
