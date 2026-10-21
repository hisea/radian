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
    Arc::new(Agent::new(model, tools).with_config(AgentConfig {
        // These tests drive the real turn loop but pin the **adapter's** one-shot behaviour:
        // ADR 0017's retry is exercised in agent-core's `retry` tests.
        retries: 0,
        ..Default::default()
    }))
    .open_session()
    .await
    .unwrap()
}

/// Calls the adapter directly so the raw `ModelError` is visible.
async fn complete_error(server: &MockServer) -> ModelError {
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    model(server).complete(request, cx).await.unwrap_err()
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
            Message::Tool { result, .. } if result.call_id == id => Some(result.text()),
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
async fn reasoning_content_streams_as_reasoning_not_text() {
    // AC-R1-8: the dogfooded shape (`research/2026-10-12-opencode-zen-dogfooding.md`
    // gap 2) - a `delta` carries `content: ""` and `reasoning_content` at the same time.
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            json!({"role": "assistant", "content": "", "reasoning_content": "The"}),
            None,
        ),
        chunk(
            json!({"content": "", "reasoning_content": " user is asking"}),
            None,
        ),
        chunk(json!({"content": "Hello!"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let model = model(&server);
    let events = Recorder::default();
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &events,
    };
    let response = model.complete(request, cx).await.unwrap();

    assert_eq!(response.content, vec![ContentPart::text("Hello!")]);
    assert_eq!(
        response.reasoning,
        vec![ContentPart::text("The user is asking")]
    );

    let (reasoning, text): (Vec<String>, Vec<String>) = {
        let events = events.0.lock().unwrap();
        let mut reasoning = Vec::new();
        let mut text = Vec::new();
        for event in events.iter() {
            match event {
                AgentEvent::ModelReasoningDelta { text: piece } => reasoning.push(piece.clone()),
                AgentEvent::ModelTextDelta { text: piece } => text.push(piece.clone()),
                _ => {}
            }
        }
        (reasoning, text)
    };
    assert_eq!(reasoning, ["The", " user is asking"]);
    // The empty `content` on the reasoning chunks must not become text deltas.
    assert_eq!(text, ["Hello!"]);
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
            Message::Tool { result, .. } => Some(result.call_id.clone()),
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
    for (code, body, transient) in [
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
        assert_eq!(model_error.is_transient(), transient, "{code}: {message}");
        assert_eq!(
            matches!(model_error, ModelError::Unavailable { .. }),
            transient,
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
        headers: Vec::new(),
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

/// ADR 0017: a `Retry-After` header is parsed and carried on the error, both as a
/// number of seconds and as an HTTP date; anything else is ignored.
#[tokio::test]
async fn a_retry_after_header_is_carried_on_the_error() {
    for (header, expected) in [
        ("2", Some(Duration::from_secs(2))),
        ("0", Some(Duration::ZERO)),
    ] {
        let server = MockServer::start(vec![Reply::Status {
            code: 429,
            headers: vec![("retry-after".into(), header.into())],
            body: "slow down".into(),
        }])
        .await;
        let error = complete_error(&server).await;
        assert_eq!(error.retry_after(), expected, "Retry-After: {header}");
        assert!(error.is_transient(), "{error}");
    }

    // An unparseable value is ignored: the core falls back to its own backoff.
    let server = MockServer::start(vec![Reply::Status {
        code: 429,
        headers: vec![("retry-after".into(), "later".into())],
        body: "slow down".into(),
    }])
    .await;
    assert_eq!(complete_error(&server).await.retry_after(), None);

    // A date in the past means "now", which is still a `Some`.
    let server = MockServer::start(vec![Reply::Status {
        code: 503,
        headers: vec![("retry-after".into(), "Sun, 06 Nov 1994 08:49:37 GMT".into())],
        body: "down".into(),
    }])
    .await;
    assert_eq!(
        complete_error(&server).await.retry_after(),
        Some(Duration::ZERO)
    );
}

/// ADR 0017: the provider's temporary failures are transient (so the core retries
/// them); a request error and a cancellation are not.
#[tokio::test]
async fn only_temporary_failures_are_transient() {
    for code in [408, 429, 500, 529] {
        let server = MockServer::start(vec![Reply::status(code, "down")]).await;
        let error = complete_error(&server).await;
        assert!(error.is_transient(), "HTTP {code}: {error}");
    }
    for code in [400, 401, 403, 404] {
        let server = MockServer::start(vec![Reply::status(code, "bad")]).await;
        let error = complete_error(&server).await;
        assert!(!error.is_transient(), "HTTP {code}: {error}");
    }

    // A stream that stops before the response is complete is a connection failure: transient.
    let server = MockServer::start(vec![Reply::SseThenCut { pieces: Vec::new() }]).await;
    let error = complete_error(&server).await;
    assert!(error.is_transient(), "{error}");

    // A cancellation is a `Transport` error that must never be retried.
    let server = MockServer::start(vec![text_reply("never arrives")]).await;
    let model = model(&server);
    let token = CancellationToken::new();
    token.cancel();
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            ModelContext {
                cancellation: token,
                events: &NoopSink,
            },
        )
        .await
        .unwrap_err();
    assert!(
        !error.is_transient(),
        "a cancellation must never retry: {error}"
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

// ---------- usage ----------

/// Calls the adapter directly so the response's usage is visible; `complete` reads the
/// streamed SSE bytes from the mock server.
async fn complete_once(
    server: &MockServer,
    tweak: impl FnOnce(&mut OpenAiCompatibleConfig),
) -> ModelResponse {
    let model = model_with(server, tweak);
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    model.complete(request, cx).await.unwrap()
}

#[tokio::test]
async fn a_trailing_usage_chunk_is_captured_through_the_sse_stream() {
    // Shape 1: everything arrives, then one final chunk with no choices carries the usage.
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            json!({ "role": "assistant", "content": "ok" }),
            Some("stop"),
        ),
        json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 4,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 3 }
            }
        }),
    ])])
    .await;

    let response = complete_once(&server, |_| {}).await;
    assert_eq!(response.content, vec![ContentPart::text("ok")]);
    let usage = response
        .usage
        .expect("the trailing usage chunk is captured");
    assert_eq!(usage.input, Some(11));
    assert_eq!(usage.output, Some(4));
    assert_eq!(usage.total, Some(15));
    assert_eq!(usage.cache_read, Some(3));
    assert_eq!(usage.cache_write, None);
    let details: Value =
        serde_json::from_str(usage.details.unwrap().get()).expect("details is JSON");
    assert_eq!(
        details,
        json!({ "prompt_tokens_details": { "cached_tokens": 3 } })
    );
}

#[tokio::test]
async fn the_user_agent_names_radian() {
    // The endpoint's own documentation asks clients to identify themselves rather than
    // impersonate a generic SDK, so this header is part of the outward contract.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut session = chat(model(&server), NoTools).await;
    session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let agent = server.requests()[0]
        .header("user-agent")
        .unwrap_or_default()
        .to_owned();
    assert!(agent.starts_with("radian/"), "{agent}");
}

#[tokio::test]
async fn usage_in_every_chunk_takes_the_last_non_empty_one() {
    // Shape 2: usage rides along on several chunks; the last non-null one wins.
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({ "role": "assistant", "content": "ok" }), None),
        json!({"choices": [], "usage": { "prompt_tokens": 1, "completion_tokens": 1 }}),
        json!({"choices": [], "usage": { "prompt_tokens": 9, "completion_tokens": 2 }}),
        json!({"choices": [], "usage": null}),
        chunk(json!({}), Some("stop")),
    ])])
    .await;

    let response = complete_once(&server, |_| {}).await;
    let usage = response.usage.expect("usage was reported");
    assert_eq!(usage.input, Some(9));
    assert_eq!(usage.output, Some(2));
}

#[tokio::test]
async fn a_response_without_usage_reports_none() {
    // Shape 3: no usage anywhere.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let response = complete_once(&server, |_| {}).await;
    assert_eq!(response.usage, None);
}

#[tokio::test]
async fn total_is_not_invented_when_the_provider_omits_it() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            json!({ "role": "assistant", "content": "ok" }),
            Some("stop"),
        ),
        json!({"choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 2 }}),
    ])])
    .await;

    let response = complete_once(&server, |_| {}).await;
    let usage = response.usage.unwrap();
    assert_eq!(usage.input, Some(7));
    assert_eq!(usage.output, Some(2));
    assert_eq!(
        usage.total, None,
        "total must stay None, never input+output"
    );
}

// ---------- include_usage on the wire ----------

#[test]
fn include_usage_defaults_to_the_openai_host_only() {
    for url in ["https://api.openai.com/v1", "https://api.openai.com:443/v1"] {
        let model = OpenAiCompatible::new(OpenAiCompatibleConfig::new(url, "m")).unwrap();
        assert!(model.include_usage(), "{url}");
    }
    for url in [
        "http://127.0.0.1:8080/v1",
        "https://evil-openai.com.attacker.net/v1",
        "https://localhost/v1",
        "https://openai.com/v1",
        "https://api.openai.com.cn/v1",
    ] {
        let model = OpenAiCompatible::new(OpenAiCompatibleConfig::new(url, "m")).unwrap();
        assert!(!model.include_usage(), "{url}");
    }
    // Explicit overrides beat the default in either direction.
    let on = OpenAiCompatible::new(
        OpenAiCompatibleConfig::new("http://127.0.0.1/v1", "m").with_include_usage(true),
    )
    .unwrap();
    assert!(on.include_usage());
    let off = OpenAiCompatible::new(
        OpenAiCompatibleConfig::new("https://api.openai.com/v1", "m").with_include_usage(false),
    )
    .unwrap();
    assert!(!off.include_usage());
}

#[tokio::test]
async fn the_request_body_carries_stream_options_only_when_enabled() {
    let check = |server: &MockServer, json: Value| {
        let body = server.requests()[0].json();
        assert_eq!(body["stream_options"], json, "body: {body}");
    };

    // Explicit Some(true) on the local mock.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut session = chat(
        model_with(&server, |c| c.include_usage = Some(true)),
        NoTools,
    )
    .await;
    session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    check(&server, json!({ "include_usage": true }));

    // Default on a local host: no stream_options key at all.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut session = chat(model(&server), NoTools).await;
    session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert!(server.requests()[0].json().get("stream_options").is_none());

    // Explicit Some(false) overrides the default and keeps the key away.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let mut session = chat(
        model_with(&server, |c| c.include_usage = Some(false)),
        NoTools,
    )
    .await;
    session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    assert!(server.requests()[0].json().get("stream_options").is_none());
}

// ---------- the 400 hint (AC-A5-5) ----------

#[tokio::test]
async fn a_400_for_stream_options_tells_you_to_turn_it_off() {
    let server = MockServer::start(vec![Reply::status(
        400,
        "{\"error\":{\"message\":\"unknown parameter: stream_options\"}}",
    )])
    .await;

    let message = run_error(model_with(&server, |c| c.include_usage = Some(true))).await;

    assert!(
        message.contains("unknown parameter: stream_options"),
        "{message}"
    );
    assert!(
        message.contains("include_usage to Some(false)"),
        "the way out is named: {message}"
    );
    assert!(!message.contains("sk-secret-key"), "{message}");
}

#[tokio::test]
async fn a_400_without_stream_options_gets_no_hint() {
    let server = MockServer::start(vec![Reply::status(400, "bad request")]).await;
    // Default policy on a local mock: stream_options was never sent.
    let message = run_error(model(&server)).await;
    assert!(message.contains("bad request"), "{message}");
    assert!(!message.contains("include_usage"), "{message}");
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
    run_model_error(model).await.to_string()
}

async fn run_model_error(model: OpenAiCompatible) -> ModelError {
    let mut session = chat(model, NoTools).await;
    let error = session
        .run_turn("hi", TurnOptions::default(), &NoopSink)
        .await
        .unwrap_err();
    let RunError::Model(model_error) = error else {
        panic!("expected a model error")
    };
    model_error
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

/// ADR 0018: a recognizable context-overflow 400 becomes `ContextOverflow`, not a generic
/// `Protocol`, so a host can tell them apart.
#[tokio::test]
async fn a_recognizable_context_overflow_is_its_own_error() {
    let body = r#"{"error":{"message":"This model's maximum context length is 16385 tokens. However, your messages resulted in 20000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let server = MockServer::start(vec![Reply::status(400, body)]).await;
    let error = run_model_error(model(&server)).await;
    assert!(
        matches!(&error, ModelError::ContextOverflow(text) if text.contains("16385")),
        "{error}"
    );
}

/// A plain 400 (no overflow code or message) stays a `Protocol` error.
#[tokio::test]
async fn a_plain_400_is_not_a_context_overflow() {
    let server = MockServer::start(vec![Reply::status(400, "bad request")]).await;
    let error = run_model_error(model(&server)).await;
    assert!(matches!(error, ModelError::Protocol(_)), "{error}");
}

/// The overflow body can echo the key like any other; it must be masked.
#[tokio::test]
async fn a_context_overflow_body_that_echoes_the_key_is_masked() {
    let body = r#"{"error":{"message":"maximum context length is 100 tokens; key sk-secret-key","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let server = MockServer::start(vec![Reply::status(400, body)]).await;
    let error = run_model_error(model(&server)).await;
    assert!(matches!(error, ModelError::ContextOverflow(_)), "{error}");
    assert!(!error.to_string().contains("sk-secret-key"), "{error}");
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

#[test]
fn urls_never_leak_credentials_through_debug_or_config_errors() {
    let url = "https://user:pass-secret@example.com/v1?token=url-secret#frag-secret";
    let config = OpenAiCompatibleConfig::new(url, "m");
    let model = OpenAiCompatible::new(config.clone()).unwrap();
    for text in [format!("{config:?}"), format!("{model:?}")] {
        for secret in ["pass-secret", "url-secret", "frag-secret", "user:"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("example.com"), "{text}");
    }

    let bad = "ftp://user:pass-secret@example.com/v1?token=url-secret";
    let error = OpenAiCompatible::new(OpenAiCompatibleConfig::new(bad, "m")).unwrap_err();
    let text = error.to_string();
    assert!(
        !text.contains("pass-secret") && !text.contains("url-secret"),
        "{text}"
    );
    let error = OpenAiCompatible::new(OpenAiCompatibleConfig::new("pass-secret", "m")).unwrap_err();
    assert!(!error.to_string().contains("pass-secret"), "{error}");
}
