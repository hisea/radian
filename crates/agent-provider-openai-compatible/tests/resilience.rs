//! The provider network-failure matrix (Phase 7) for the OpenAI-compatible adapter: what it
//! does **today**, recorded rather than changed. The summary table lives in
//! `crates/agent-provider-contract/README.md`.
//!
//! Timing tests assert the *result*, never a duration; the only clock is a hang watchdog.

use std::sync::Mutex;
use std::time::Duration;

use radian_core::{
    AgentEvent, CancellationToken, ContentPart, EventSink, Message, Model, ModelContext,
    ModelError, ModelRequest, ModelResponse, NoopSink,
};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use serde_json::{Value, json};

const SECRET: &str = "sk-openai-resilience-secret";
const HEADER_VALUE: &str = "resilience-header-secret";

fn model_with(
    server: &MockServer,
    tweak: impl FnOnce(&mut OpenAiCompatibleConfig),
) -> OpenAiCompatible {
    let mut config = OpenAiCompatibleConfig::new(&server.base_url, "test-model")
        .with_api_key(ApiKey::new(SECRET));
    config.headers.push(("x-team".into(), HEADER_VALUE.into()));
    tweak(&mut config);
    OpenAiCompatible::new(config).expect("a valid config")
}

fn model(server: &MockServer) -> OpenAiCompatible {
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

async fn complete(
    model: &OpenAiCompatible,
    events: &dyn EventSink,
) -> Result<ModelResponse, ModelError> {
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

fn pieces(chunks: &[Value]) -> Vec<Vec<u8>> {
    chunks
        .iter()
        .map(|c| format!("data: {c}\n\n").into_bytes())
        .collect()
}

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
    assert!(message.contains(&"a".repeat(2048)));
    assert!(!message.contains(&"a".repeat(2049)));
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
    assert_eq!(events.text(), "");
}

#[tokio::test]
async fn a_connection_cut_after_deltas_is_a_transport_error_with_no_partial_answer() {
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: pieces(&[chunk(json!({"content": "half an ans"}), None)]),
    }])
    .await;
    let events = Recorder::default();
    let result = complete(&model(&server), &events).await;
    assert_eq!(events.text(), "half an ans");
    assert!(
        matches!(result, Err(ModelError::Transport(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_early_eof_after_deltas_is_a_transport_error() {
    // A clean end of the body with no finish reason and no `[DONE]`.
    let server = MockServer::start(vec![Reply::Sse {
        pieces: pieces(&[chunk(json!({"content": "cut"}), None)]),
        pause: Duration::ZERO,
        headers: Vec::new(),
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
        pieces: pieces(&[chunk(json!({"content": "start"}), None)]),
    }])
    .await;
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
        headers: Vec::new(),
    }])
    .await;
    let error = complete(&model(&server), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Protocol(m) if m.contains("unreadable stream chunk")),
        "{error}"
    );
}

#[tokio::test]
async fn an_unknown_field_is_ignored() {
    let mut chunks = vec![chunk(
        json!({"role": "assistant", "content": "ok", "future_field": {"x": 1}}),
        None,
    )];
    chunks.push(chunk(json!({}), Some("stop")));
    let server = MockServer::start(vec![Reply::sse(&chunks)]).await;
    let response = complete(&model(&server), &NoopSink).await.unwrap();
    assert_eq!(content_text(&response), "ok");
}

#[tokio::test]
async fn a_chunk_without_choices_is_skipped() {
    // A usage-only trailing chunk (no `choices`) is recorded and skipped, not an error.
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        json!({"id": "chatcmpl-test", "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let response = complete(&model(&server), &NoopSink).await.unwrap();
    assert_eq!(content_text(&response), "ok");
    assert_eq!(response.usage.as_ref().and_then(|u| u.input), Some(1));
}

#[tokio::test]
async fn a_provider_error_chunk_is_a_transport_error() {
    let server = MockServer::start(vec![Reply::sse(&[json!({
        "error": {"message": "busy right now", "type": "server_error"}
    })])])
    .await;
    let error = complete(&model(&server), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("busy right now")),
        "{error}"
    );
}

#[tokio::test]
async fn a_stream_error_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::sse(&[json!({
        "error": {"message": format!("bad key {SECRET}"), "type": "authentication_error"}
    })])])
    .await;
    let message = complete(&model(&server), &NoopSink)
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("<redacted>"), "{message}");
    assert!(!message.contains(SECRET), "{message}");
}
