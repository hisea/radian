//! A steer accepted during a turn that then ends without applying it must not be lost
//! (ADR 0011, ADR 0017).
//!
//! `drain_steering` hands a `next_step` input to Core as soon as it can; Core only applies it
//! at the next safe point. On an `end_turn` finish Core keeps the turn going when steering is
//! pending, but a model error ends the turn with the steer still unapplied. The entry was
//! claimed but never admitted; the driver puts it back at the front of its lane, and (ADR 0011)
//! that requeued waking entry opens the next turn by itself, so the steer is used exactly once
//! without a followup.
//!
//! A `max_tokens` finish no longer ends the turn (ADR 0017 §2): with no tool calls the core
//! appends the output-limit continuation notice and makes another model call, and the next
//! step's safe point admits the steer into that continuation request. The order in the
//! transcript is the truncated answer, the notice (committed with the response), then the steer.
//! One turn, three model calls, and the steer is still used exactly once.

mod common;

use std::sync::Arc;
use std::time::Duration;

use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, input_json_delta, message_delta, message_start, message_stop,
    text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use common::{MemTools, Recorder, TempDir, created};
use radian_core::{Agent, AgentEvent, EventSink, InputSource, RunStatus, TurnOptions};
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
/// the call. Core continues the turn with the notice (ADR 0017 §2), so the steer is admitted
/// by the continuation request.
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
        headers: Vec::new(),
    }
}

/// The continuation notice, verbatim (ADR 0017 §2).
const MAX_TOKENS_NOTICE: &str = "Your last response hit the output token limit. Do not apologize, recap, or repeat yourself. Continue from where you stopped, and break the remaining work into smaller pieces.";

/// Waits for one turn to finish. A scenario that scripts too few replies for the turn it
/// expects fails here instead of hanging until nextest's timeout.
async fn wait_turn(recorder: &Recorder) {
    tokio::time::timeout(Duration::from_secs(30), recorder.wait_finished())
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

/// Opens a driver over a real session and runs the scenario: turn 1 calls the `gate` tool and
/// the steer is injected while the second model call is in flight. What ends the turn depends
/// on `second_reply`: a model error requeues the steer and the next turn carries it, while a
/// `max_tokens` finish continues the same turn with the notice and the steer rides the
/// continuation. `turns` is how many turns finish; the third scripted reply ends either shape.
/// Returns the request bodies, the session history and turn 1's status.
async fn run_scenario(
    label: &str,
    second_reply: Reply,
    idle_timeout: Option<Duration>,
    turns: usize,
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
        Agent::new(Anthropic::new(config).unwrap(), tools).with_config(radian_core::AgentConfig {
            // The scenario drives a failing turn (a transient idle timeout); pin the
            // pre-revision one-shot behaviour so the retry does not consume the next scripted
            // reply (ADR 0017's retry is covered in radian-core's `retry` tests).
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
    // in flight, after the `inject_steering` that preceded that call. The next safe point
    // (a `max_tokens` continuation step, or the next turn after a model error) admits it.
    inbox.followup("go", InputSource::User).await.unwrap();
    started.notified().await;
    gate.notify_one();
    while server.requests().len() < 2 {
        tokio::task::yield_now().await;
    }
    inbox.steer("STEER-ME", InputSource::User).await.unwrap();
    wait_turn(&recorder).await;
    let first_status = last_status(&recorder).expect("a turn finished");

    if turns > 1 {
        // ADR 0011: the requeued steer opens the next turn itself, as its first
        // input — no followup is needed to pick it up.
        wait_turn(&recorder).await;
        assert_eq!(last_status(&recorder), Some(RunStatus::Completed));
    }
    inbox.close();
    handle.await.unwrap().expect("the driver survives");

    let requests: Vec<String> = server.requests().into_iter().map(|r| r.body).collect();
    assert_eq!(requests.len(), 3, "three model calls expected");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.matches("STEER-ME").count())
            .sum::<usize>(),
        1,
        "the steer must appear in exactly one request: {requests:?}"
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
        2,
    )
    .await;
    assert_eq!(first_status, RunStatus::Failed);
    // The first two calls were turn 1's; the third (turn 2) carried the steer.
    assert!(requests[0].contains("go"), "{}", requests[0]);
    assert!(requests[1].contains("tool_result"), "{}", requests[1]);
    assert!(history.contains(&"SECOND-OK".to_string()), "{history:?}");
}

/// A `max_tokens` finish used to end the turn with the steer unapplied. Now the turn continues
/// (ADR 0017 §2), and the next safe point admits the steer into the continuation request —
/// exactly once, never dropped. The notice is committed with the truncated response, so it
/// precedes the steer in the transcript and in the request (ADR 0011 L1).
#[tokio::test]
async fn a_steer_accepted_during_a_max_tokens_finish_reaches_the_continuation_once() {
    let (requests, history, first_status) =
        run_scenario("steer-max-tokens", slow_max_tokens_reply("FINAL"), None, 1).await;
    assert_eq!(
        first_status,
        RunStatus::Completed,
        "the continuation notice keeps the turn going"
    );
    assert!(history.contains(&"SECOND-OK".to_string()), "{history:?}");
    assert_eq!(
        history.iter().filter(|text| *text == "STEER-ME").count(),
        1,
        "the steer is admitted exactly once: {history:?}"
    );

    // The continuation request carries the truncated answer, then the notice, then the steer.
    let continuation = &requests[2];
    assert!(continuation.contains("FINAL"), "{continuation}");
    let notice = continuation
        .find(MAX_TOKENS_NOTICE)
        .expect("the continuation request carries the notice");
    let steer = continuation
        .find("STEER-ME")
        .expect("the continuation request carries the steer");
    assert!(
        notice < steer,
        "the notice precedes the steer in the continuation request: {continuation}"
    );
    assert_eq!(continuation.matches(MAX_TOKENS_NOTICE).count(), 1);
}
