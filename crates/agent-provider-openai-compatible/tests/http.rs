//! The adapter against a mock server, driven through the real core turn loop.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::*;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use agent_provider_openai_compatible::{
    ApiKey, ConfigError, OpenAiCompatible, OpenAiCompatibleConfig,
};
use serde_json::{Value, json};

// ---------- helpers ----------

fn model(server: &MockServer) -> OpenAiCompatible {
    model_with(server, |_| {})
}

fn model_with(
    server: &MockServer,
    tweak: impl FnOnce(&mut OpenAiCompatibleConfig),
) -> OpenAiCompatible {
    let mut config = OpenAiCompatibleConfig::new(&server.base_url, "test-model")
        .with_api_key(ApiKey::new("sk-secret-key"));
    tweak(&mut config);
    OpenAiCompatible::new(config).expect("a valid config")
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

async fn chat<M: Model, P: ToolProvider>(model: M, tools: P) -> AgentSession<M, P, ()> {
    Arc::new(Agent::new(model, tools))
        .open_session()
        .await
        .unwrap()
}

/// One tool, `add`, that sums two integers.
struct Calc;

struct CalcSession(Vec<ToolSpec>);

impl ToolProvider for Calc {
    type Session = CalcSession;

    async fn open_session(&self) -> Result<CalcSession, ToolError> {
        Ok(CalcSession(vec![ToolSpec {
            name: "add".into(),
            description: "Adds two integers.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
                "required": ["a", "b"]
            }),
            concurrency: ToolConcurrency::Shared,
        }]))
    }
}

impl ToolSession for CalcSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.0
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let n = |k: &str| call.arguments[k].as_i64().unwrap_or_default();
        Ok(ToolOutput::text((n("a") + n("b")).to_string()))
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<AgentEvent>>);

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn tool_result_text(transcript: &[Message], id: &str) -> String {
    transcript
        .iter()
        .find_map(|m| match m {
            Message::Tool { result } if result.call_id == id => Some(result.text()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

// ---------- the request ----------

#[tokio::test]
async fn a_turn_sends_the_expected_request_and_returns_the_text() {
    let server = MockServer::start(vec![text_reply("Hello there")]).await;
    let mut session = chat(
        model_with(&server, |c| {
            c.headers.push(("x-team".into(), "radian".into()));
            c.extra_body.insert("temperature".into(), json!(0.3));
        }),
        NoTools,
    )
    .await;
    let events = Recorder::default();

    let outcome = session
        .run_turn("hi", TurnOptions::default(), &events)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "Hello there");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.header("authorization"),
        Some("Bearer sk-secret-key")
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("x-team"), Some("radian"));
    assert_eq!(
        request.json(),
        json!({
            "model": "test-model", "stream": true, "temperature": 0.3,
            "messages": [{"role": "user", "content": "hi"}]
        })
    );
    // The text was streamed as events too.
    assert!(
        events
            .0
            .lock()
            .unwrap()
            .contains(&AgentEvent::ModelTextDelta {
                text: "Hello there".into()
            })
    );
}

#[tokio::test]
async fn a_base_url_with_a_trailing_slash_or_no_path_works() {
    for (suffix, expected) in [("/v1/", "/v1/chat/completions"), ("", "/chat/completions")] {
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let base = server.base_url.trim_end_matches("/v1").to_owned();
        let url = if suffix.is_empty() {
            base
        } else {
            format!("{base}{suffix}")
        };
        let model = OpenAiCompatible::new(OpenAiCompatibleConfig::new(url, "m")).unwrap();
        let mut session = chat(model, NoTools).await;

        session
            .run_turn("hi", TurnOptions::default(), &NoopSink)
            .await
            .unwrap();

        assert_eq!(server.requests()[0].path, expected);
    }
}

// ---------- streaming ----------

#[tokio::test]
async fn a_response_split_into_tiny_pieces_is_read_the_same() {
    let chunks = [
        chunk(json!({"role": "assistant", "content": "你好，"}), None),
        chunk(json!({"content": "世界 😀"}), None),
        chunk(json!({}), Some("stop")),
    ];
    for size in [1, 2, 3, 7] {
        let server = MockServer::start(vec![Reply::sse_split(&chunks, size)]).await;
        let mut session = chat(model(&server), NoTools).await;

        let outcome = session
            .run_turn("hi", TurnOptions::default(), &NoopSink)
            .await
            .unwrap();

        assert_eq!(
            outcome.final_message.unwrap().text(),
            "你好，世界 😀",
            "pieces of {size} bytes"
        );
    }
}

#[tokio::test]
async fn a_tool_call_round_trips_through_the_core() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "add", "{\"a\": 2, \"b\": 3}"),
        text_reply("The sum is 5."),
    ])
    .await;
    let mut session = chat(model(&server), Calc).await;

    let outcome = session
        .run_turn("2+3?", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(tool_result_text(session.transcript(), "call_1"), "5");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    // The first request offered the tool.
    assert_eq!(requests[0].json()["tools"][0]["function"]["name"], "add");
    // The second carried the assistant's call and the tool's result, with matching ids.
    let messages = requests[1].json()["messages"].clone();
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["arguments"],
        "{\"a\":2,\"b\":3}"
    );
    assert_eq!(
        messages[2],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "5"})
    );
}

#[tokio::test]
async fn reasoning_text_the_server_streams_is_ignored() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"reasoning_content": "let me think"}), None),
        chunk(json!({"content": "answer"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let mut session = chat(model(&server), NoTools).await;

    let outcome = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.final_message.unwrap().text(), "answer");
}

#[tokio::test]
async fn a_server_that_reuses_tool_call_ids_does_not_break_the_conversation() {
    let server = MockServer::start(vec![
        tool_reply("call_0", "add", "{\"a\": 1, \"b\": 1}"),
        tool_reply("call_0", "add", "{\"a\": 2, \"b\": 2}"),
        text_reply("done"),
    ])
    .await;
    let provider = Calc;
    let mut session = chat(model(&server), provider).await;

    let outcome = session
        .run_turn("go", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    let ids: Vec<String> = session
        .transcript()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { result } => Some(result.call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["call_0", "call_0-2"]);
    // The history is valid, so it can be used to open a new session.
    let history = session.history_for_resume().history;
    assert!(
        Arc::new(Agent::new(model(&server), Calc))
            .open_session_with_history(history)
            .await
            .is_ok()
    );
}

// ---------- failures ----------

#[tokio::test]
async fn http_errors_are_classified_and_never_show_the_key() {
    for (code, body, transport) in [
        (401, "{\"error\":{\"message\":\"bad key\"}}", false),
        (400, "{\"error\":{\"message\":\"bad request\"}}", false),
        (408, "timeout", true),
        (429, "{\"error\":{\"message\":\"slow down\"}}", true),
        (500, "oops", true),
        (503, "busy", true),
    ] {
        let server = MockServer::start(vec![Reply::status(code, body)]).await;
        let mut session = chat(model(&server), NoTools).await;

        let error = session
            .run_turn("hi", TurnOptions::default(), &NoopSink)
            .await
            .unwrap_err();

        let RunError::Model(model_error) = error else {
            panic!("expected a model error for {code}")
        };
        let message = model_error.to_string();
        assert_eq!(
            matches!(model_error, ModelError::Transport(_)),
            transport,
            "{code}: {message}"
        );
        assert!(message.contains(&format!("HTTP {code}")), "{message}");
        assert!(message.contains(body), "the body is quoted: {message}");
        assert!(!message.contains("sk-secret-key"));
    }
}

#[tokio::test]
async fn a_cut_connection_gives_an_error_and_no_partial_answer() {
    let server = MockServer::start(vec![Reply::SseThenCut {
        pieces: vec![
            format!(
                "data: {}\n\n",
                chunk(json!({"content": "half an ans"}), None)
            )
            .into_bytes(),
        ],
    }])
    .await;
    let mut session = chat(model(&server), NoTools).await;

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(
        matches!(error, RunError::Model(ModelError::Transport(_))),
        "{error}"
    );
    // Only the user message is in the transcript: the half answer was not kept.
    assert_eq!(session.transcript().len(), 1);
}

#[tokio::test]
async fn a_stream_that_ends_before_finishing_is_an_error() {
    let server = MockServer::start(vec![Reply::Sse {
        pieces: vec![format!("data: {}\n\n", chunk(json!({"content": "cut"}), None)).into_bytes()],
        pause: Duration::ZERO,
    }])
    .await;
    let mut session = chat(model(&server), NoTools).await;

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RunError::Model(ModelError::Transport(m)) if m.contains("complete")),
        "{error}"
    );
}

#[tokio::test]
async fn a_silent_server_hits_the_idle_timeout() {
    let server = MockServer::start(vec![Reply::SseThenHang {
        pieces: vec![
            format!("data: {}\n\n", chunk(json!({"content": "so far"}), None)).into_bytes(),
        ],
    }])
    .await;
    let mut session = chat(
        model_with(&server, |c| c.idle_timeout = Duration::from_millis(300)),
        NoTools,
    )
    .await;
    let start = Instant::now();

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RunError::Model(ModelError::Transport(m)) if m.contains("nothing")),
        "{error}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn cancelling_stops_a_request_that_is_waiting_on_the_server() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let mut session = chat(model(&server), NoTools).await;
    let mut run = session.start_turn("hi").unwrap();
    let cancel = run.cancellation_handle();
    let start = Instant::now();

    let (outcome, ()) = tokio::join!(run.run(&NoopSink), async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });

    assert_eq!(
        outcome.unwrap().status,
        RunStatus::Cancelled(CancelReason::Requested)
    );
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn a_model_called_directly_honours_the_cancellation_token() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let model = model(&server);
    let token = CancellationToken::new();
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: token.clone(),
        events: &NoopSink,
    };

    let (result, ()) = tokio::join!(model.complete(request, cx), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });

    assert!(matches!(result, Err(ModelError::Transport(m)) if m.contains("cancelled")));
}

#[tokio::test]
async fn a_response_over_the_size_limit_is_refused() {
    let big = "x".repeat(2000);
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"content": big}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let mut session = chat(
        model_with(&server, |c| c.max_response_bytes = 1000),
        NoTools,
    )
    .await;

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(
        matches!(error, RunError::Model(ModelError::Protocol(_))),
        "{error}"
    );
}

#[tokio::test]
async fn a_redirect_is_not_followed_so_the_key_stays_put() {
    let server = MockServer::start(vec![
        Reply::Status {
            code: 307,
            headers: vec![("location".into(), "/v1/chat/completions".into())],
            body: String::new(),
        },
        text_reply("you should not get this"),
    ])
    .await;
    let mut session = chat(model(&server), NoTools).await;

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RunError::Model(m) if m.to_string().contains("HTTP 307")),
        "{error}"
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_connection_error_is_a_transport_error_without_the_url() {
    // Nothing listens on this port.
    let model =
        OpenAiCompatible::new(OpenAiCompatibleConfig::new("http://127.0.0.1:1/v1", "m")).unwrap();
    let mut session = chat(model, NoTools).await;

    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();

    let RunError::Model(ModelError::Transport(message)) = error else {
        panic!("expected a transport error")
    };
    assert!(!message.contains("127.0.0.1:1"), "{message}");
}

// ---------- configuration ----------

#[test]
fn a_key_is_never_sent_over_plain_http_to_a_remote_host() {
    let remote = OpenAiCompatibleConfig::new("http://api.example.com/v1", "m")
        .with_api_key(ApiKey::new("k"));
    assert!(matches!(
        OpenAiCompatible::new(remote),
        Err(ConfigError::InsecureKey { .. })
    ));

    for local in [
        "http://localhost:8080/v1",
        "http://127.0.0.1/v1",
        "http://[::1]:11434/v1",
    ] {
        let config = OpenAiCompatibleConfig::new(local, "m").with_api_key(ApiKey::new("k"));
        assert!(OpenAiCompatible::new(config).is_ok(), "{local}");
    }
    let https = OpenAiCompatibleConfig::new("https://api.example.com/v1", "m")
        .with_api_key(ApiKey::new("k"));
    assert!(OpenAiCompatible::new(https).is_ok());
    // Without a key, plain http to a remote host is the host's own business.
    assert!(
        OpenAiCompatible::new(OpenAiCompatibleConfig::new(
            "http://api.example.com/v1",
            "m"
        ))
        .is_ok()
    );
}

#[test]
fn bad_configuration_is_reported() {
    for url in ["not a url", "ftp://example.com/v1", "file:///etc/passwd"] {
        let result = OpenAiCompatible::new(OpenAiCompatibleConfig::new(url, "m"));
        assert!(matches!(result, Err(ConfigError::BaseUrl { .. })), "{url}");
    }
    let mut config = OpenAiCompatibleConfig::new("https://example.com/v1", "m");
    config.headers.push(("bad header name".into(), "v".into()));
    assert!(matches!(
        OpenAiCompatible::new(config),
        Err(ConfigError::Header(_))
    ));
}

#[test]
fn the_key_is_hidden_from_debug_output() {
    let config = OpenAiCompatibleConfig::new("https://example.com/v1", "m")
        .with_api_key(ApiKey::new("sk-secret-key"));
    let model = OpenAiCompatible::new(config.clone()).unwrap();
    assert!(!format!("{config:?}").contains("sk-secret-key"));
    assert!(!format!("{model:?}").contains("sk-secret-key"));

    let mut config = config;
    config
        .headers
        .push(("X-Api-Key".into(), "hdr-secret-value".into()));
    let model = OpenAiCompatible::new(config).unwrap();
    assert!(!format!("{model:?}").contains("hdr-secret-value"));
}

// ---------- secrets in errors ----------

async fn run_error(model: OpenAiCompatible) -> String {
    let mut session = chat(model, NoTools).await;
    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();
    let RunError::Model(model_error) = error else {
        panic!("expected a model error")
    };
    model_error.to_string()
}

#[tokio::test]
async fn an_http_error_body_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"Incorrect API key provided: sk-secret-key\"}}",
    )])
    .await;

    let message = run_error(model(&server)).await;

    assert!(message.contains("Incorrect API key provided"), "{message}");
    assert!(!message.contains("sk-secret-key"), "{message}");
}

#[tokio::test]
async fn a_stream_error_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::sse(&[serde_json::json!({
        "error": {"message": "rejected key sk-secret-key"}
    })])])
    .await;

    let message = run_error(model(&server)).await;

    assert!(message.contains("rejected key"), "{message}");
    assert!(!message.contains("sk-secret-key"), "{message}");
}

#[tokio::test]
async fn a_custom_header_value_echoed_by_the_server_is_masked() {
    let server = MockServer::start(vec![Reply::status(403, "denied for hdr-secret-value")]).await;

    let message = run_error(model_with(&server, |config| {
        config
            .headers
            .push(("X-Api-Key".into(), "hdr-secret-value".into()));
    }))
    .await;

    assert!(message.contains("denied for"), "{message}");
    assert!(!message.contains("hdr-secret-value"), "{message}");
}

#[test]
fn debug_output_hides_the_key_and_header_values() {
    let mut config = OpenAiCompatibleConfig::new("https://example.com/v1", "m")
        .with_api_key(ApiKey::new("sk-secret-key"));
    config
        .headers
        .push(("X-Api-Key".into(), "hdr-secret-value".into()));

    let text = format!("{config:?}");

    assert!(text.contains("X-Api-Key"), "names stay visible: {text}");
    assert!(!text.contains("sk-secret-key"), "{text}");
    assert!(!text.contains("hdr-secret-value"), "{text}");
}
