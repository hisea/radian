//! The provider network-failure matrix (Phase 7): what the adapter does **today**, recorded
//! rather than changed. The summary table lives in `docs/provider-contract.md`.
//!
//! Every test drives the adapter's public `Model::complete` against the mock server. Timing
//! tests assert the *result* (an error of a given kind), never a duration; the only clock is
//! `tokio::time::timeout` used as a hang watchdog.

use std::sync::Mutex;
use std::time::Duration;

use agent_core::{
    AgentEvent, CancellationToken, ContentPart, EventSink, Message, Model, ModelContext,
    ModelError, ModelRequest, ModelResponse, NoopSink,
};
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, message_delta, message_start, message_stop, text_delta,
    text_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use serde_json::{Value, json};

const SECRET: &str = "sk-ant-resilience-secret";
const HEADER_VALUE: &str = "resilience-header-secret";

fn model_with(server: &MockServer, tweak: impl FnOnce(&mut AnthropicConfig)) -> Anthropic {
    let mut config =
        AnthropicConfig::new(&server.base_url, "claude-test").with_api_key(ApiKey::new(SECRET));
    config.headers.push(("x-team".into(), HEADER_VALUE.into()));
    tweak(&mut config);
    Anthropic::new(config).expect("a local mock config is valid")
}

fn model(server: &MockServer) -> Anthropic {
    model_with(server, |_| {})
}

fn request() -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    }
}

async fn complete(model: &Anthropic, events: &dyn EventSink) -> Result<ModelResponse, ModelError> {
    model
        .complete(
            request(),
            ModelContext {
                cancellation: CancellationToken::new(),
                events,
            },
        )
        .await
}

/// SSE wire bytes for a list of event objects.
fn content_text(response: &ModelResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// SSE wire bytes for a list of event objects.
fn pieces(events: &[Value]) -> Vec<Vec<u8>> {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n").into_bytes())
        .collect()
}

fn text_events(text: &str) -> Vec<Value> {
    vec![
        message_start(None),
        text_start(0),
        text_delta(0, text),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ]
}

/// Records the live events so a test can see what was streamed before a failure.
#[derive(Default)]
struct Recorder(Mutex<Vec<AgentEvent>>);

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Recorder {
    fn text(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelTextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }
}

// ---------- 1. HTTP status matrix ----------

#[tokio::test]
async fn http_status_matrix_is_classified_and_never_shows_a_secret() {
    // 5xx, 408 and 429 are the provider's (`Unavailable`, transient); the rest are the
    // request's (`Protocol`, never retried).
    for (code, transient) in [
        (400, false),
        (401, false),
        (403, false),
        (404, false),
        (408, true),
        (429, true),
        (500, true),
        (503, true),
    ] {
        let server = MockServer::start(vec![Reply::status(code, "provider-body-text")]).await;
        let error = complete(&model(&server), &NoopSink).await.unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&format!("HTTP {code}")),
            "{code}: {message}"
        );
        assert!(message.contains("provider-body-text"), "{code}: {message}");
        assert!(
            !message.contains(SECRET),
            "{code} leaked the key: {message}"
        );
        assert!(
            !message.contains(HEADER_VALUE),
            "{code} leaked a header value: {message}"
        );
        assert_eq!(error.is_transient(), transient, "{code}");
        if transient {
            assert!(
                matches!(error, ModelError::Unavailable { .. }),
                "{code} must be Unavailable: {error}"
            );
        } else {
            assert!(
                matches!(error, ModelError::Protocol(_)),
                "{code} must be Protocol: {error}"
            );
        }
    }
}

#[tokio::test]
async fn a_long_error_body_is_truncated_to_2048_bytes() {
    let server = MockServer::start(vec![Reply::status(500, &"a".repeat(5000))]).await;
    let message = complete(&model(&server), &NoopSink)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains(&"a".repeat(2048)),
        "the first 2048 bytes are kept"
    );
    assert!(
        !message.contains(&"a".repeat(2049)),
        "the body must be truncated at 2048 bytes"
    );
}

#[tokio::test]
async fn an_empty_error_body_says_so() {
    let server = MockServer::start(vec![Reply::status(500, "")]).await;
    let message = complete(&model(&server), &NoopSink)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("(empty body)"), "{message}");
}

// ---------- 2. the stream ends early ----------

#[tokio::test]
async fn a_connection_cut_before_the_first_delta_is_a_transport_error() {
    let server = MockServer::start(vec![Reply::SseThenCut { pieces: Vec::new() }]).await;
    let events = Recorder::default();
    let error = complete(&model(&server), &events).await.unwrap_err();
    assert!(matches!(error, ModelError::Transport(_)), "{error}");
    assert_eq!(events.text(), "", "nothing was streamed");
}

#[tokio::test]
async fn a_connection_cut_after_deltas_is_a_transport_error_with_no_partial_answer() {
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: pieces(&[
            message_start(None),
            text_start(0),
            text_delta(0, "half an ans"),
        ]),
    }])
    .await;
    let events = Recorder::default();
    let result = complete(&model(&server), &events).await;
    // The partial text reached the live event sink ...
    assert_eq!(events.text(), "half an ans");
    // ... but `complete` did not turn it into a `ModelResponse`: the core sees an error.
    assert!(
        matches!(result, Err(ModelError::Transport(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_early_eof_after_deltas_is_a_transport_error() {
    // A clean end of the body, but no `message_stop`: the stream is incomplete.
    let server = MockServer::start(vec![Reply::Sse {
        pieces: pieces(&[message_start(None), text_start(0), text_delta(0, "cut")]),
        pause: Duration::ZERO,
    }])
    .await;
    let events = Recorder::default();
    let error = complete(&model(&server), &events).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("complete")),
        "{error}"
    );
    assert_eq!(events.text(), "cut");
}

// ---------- 3. idle timeout ----------

#[tokio::test]
async fn a_silent_server_hits_the_idle_timeout() {
    let server = MockServer::start(vec![Reply::SseThenHang {
        pieces: pieces(&[message_start(None), text_start(0), text_delta(0, "start")]),
    }])
    .await;
    // The only clock: a small idle timeout, so the test does not wait long.
    let model = model_with(&server, |config| {
        config.idle_timeout = Duration::from_millis(50);
    });
    // The watchdog is a failure bound, not a measurement: without the idle timeout the
    // request would hang forever.
    let error = tokio::time::timeout(Duration::from_secs(5), complete(&model, &NoopSink))
        .await
        .expect("the idle timeout must fire, not hang")
        .unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("sent nothing")),
        "{error}"
    );
}

// ---------- 4. cancellation ----------

#[tokio::test]
async fn cancelling_returns_instead_of_hanging() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let model = model(&server);
    let token = CancellationToken::new();
    let run = model.complete(
        request(),
        ModelContext {
            cancellation: token.clone(),
            events: &NoopSink,
        },
    );
    let cancel = async {
        token.cancel();
    };
    // The watchdog is a failure bound, not a measurement: `complete` must return on its own.
    let joined = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
        .await
        .expect("complete must return on cancellation, not hang");
    let (result, ()) = joined;
    let error = result.unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("cancelled")),
        "{error}"
    );
}

// ---------- 5. malformed SSE ----------

#[tokio::test]
async fn a_non_json_data_line_is_a_protocol_error() {
    let server = MockServer::start(vec![Reply::Sse {
        pieces: vec![b"data: this is not json\n\n".to_vec()],
        pause: Duration::ZERO,
    }])
    .await;
    let error = complete(&model(&server), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Protocol(m) if m.contains("unreadable stream event")),
        "{error}"
    );
}

#[tokio::test]
async fn an_unknown_event_type_is_ignored() {
    let mut events = vec![json!({"type": "a_future_event", "payload": 1})];
    events.extend(text_events("ok"));
    let server = MockServer::start(vec![Reply::sse(&events)]).await;
    let response = complete(&model(&server), &NoopSink).await.unwrap();
    assert_eq!(content_text(&response), "ok");
}

#[tokio::test]
async fn missing_fields_are_tolerated() {
    // `content_block_start` with no `content_block`: the block is ignored, not an error.
    let mut events = vec![json!({"type": "content_block_start", "index": 0})];
    events.extend(text_events("ok"));
    let server = MockServer::start(vec![Reply::sse(&events)]).await;
    let response = complete(&model(&server), &NoopSink).await.unwrap();
    assert_eq!(content_text(&response), "ok");
}

#[tokio::test]
async fn a_provider_error_event_is_a_transport_error() {
    let server = MockServer::start(vec![Reply::sse(&[json!({
        "type": "error",
        "error": {"type": "overloaded_error", "message": "busy right now"}
    })])])
    .await;
    let error = complete(&model(&server), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("busy right now")),
        "{error}"
    );
}

/// A stream error that echoes the key is masked (item 1's "never shows the key", stream path).
#[tokio::test]
async fn a_stream_error_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::sse(&[json!({
        "type": "error",
        "error": {"type": "authentication_error", "message": format!("bad key {SECRET}")}
    })])])
    .await;
    let message = complete(&model(&server), &NoopSink)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("<redacted>"), "{message}");
    assert!(!message.contains(SECRET), "{message}");
}
