//! The Anthropic adapter against a mock server: request shape, the event stream, errors,
//! timeouts, usage and secret handling. Driven through the real core turn loop where it
//! matters, and directly through `complete` where the response's usage must be visible.
//!
//! All event shapes come from the official documentation (see
//! `docs/providers/anthropic-wire-notes.md`); this file adds nothing to the wire format.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::*;
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, error, input_json_delta, message_delta, message_start,
    message_stop, ping, text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey, ConfigError, ThinkingConfig};
use serde_json::{Value, json};

// ---------- helpers ----------

fn model(server: &MockServer) -> Anthropic {
    model_with(server, |_| {})
}

fn model_with(server: &MockServer, tweak: impl FnOnce(&mut AnthropicConfig)) -> Anthropic {
    let mut config = AnthropicConfig::new(&server.base_url, "claude-test")
        .with_api_key(ApiKey::new("sk-ant-secret-key"));
    tweak(&mut config);
    Anthropic::new(config).expect("a valid config")
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        text_start(0),
        text_delta(0, text),
        block_stop(0),
        message_delta("end_turn", Some(json!({"output_tokens": 2}))),
        message_stop(),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        message_start(None),
        tool_start(0, Some(id), name, json!({})),
        input_json_delta(0, arguments),
        block_stop(0),
        message_delta("tool_use", None),
        message_stop(),
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
            Message::Tool { result, .. } if result.call_id == id => Some(result.text()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no result for {id}"))
}

/// Calls the adapter directly so the response's usage is visible.
async fn complete_once(
    server: &MockServer,
    tweak: impl FnOnce(&mut AnthropicConfig),
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
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.header("x-api-key"), Some("sk-ant-secret-key"));
    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("accept"), Some("text/event-stream"));
    assert_eq!(request.header("x-team"), Some("radian"));

    let body = request.json();
    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["stream"], true);
    assert_eq!(body["temperature"], 0.3);
    assert_eq!(
        body["messages"],
        json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
    );
    assert!(body.get("system").is_none());

    // The text streamed as deltas in order, and the turn saw them.
    let deltas: Vec<String> = events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ModelTextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["Hello there"]);
}

#[tokio::test]
async fn the_user_agent_names_radian() {
    // Same outward contract as the OpenAI-compatible adapter: identify as radian, not as a
    // generic SDK.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let _ = complete_once(&server, |_| {}).await;
    let agent = server.requests()[0]
        .header("user-agent")
        .unwrap_or_default()
        .to_owned();
    assert!(agent.starts_with("radian/"), "{agent}");
}

#[tokio::test]
async fn a_base_url_with_a_trailing_slash_or_a_path_works() {
    let server = MockServer::start(vec![text_reply("ok"), text_reply("ok")]).await;
    for base in [
        format!("{}/", server.base_url),
        format!("{}/proxy", server.base_url),
    ] {
        let config = AnthropicConfig::new(base, "m").with_api_key(ApiKey::new("k"));
        let model = Anthropic::new(config).unwrap();
        let cx = ModelContext {
            cancellation: CancellationToken::new(),
            events: &NoopSink,
        };
        let response = model
            .complete(
                ModelRequest {
                    system_prompt: None,
                    messages: vec![Message::user("hi")],
                    tools: Vec::new(),
                    settings: Value::Null,
                },
                cx,
            )
            .await
            .unwrap();
        assert_eq!(response.content, vec![ContentPart::text("ok")]);
    }
    let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
    assert_eq!(paths, ["/v1/messages", "/proxy/v1/messages"]);
}

#[tokio::test]
async fn the_anthropic_version_header_defaults_and_can_be_overridden() {
    let server = MockServer::start(vec![text_reply("ok"), text_reply("ok")]).await;
    let _ = complete_once(&server, |_| {}).await;
    let _ = complete_once(&server, |c| {
        c.anthropic_version = "2023-01-01".into();
    })
    .await;
    let requests = server.requests();
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(requests[1].header("anthropic-version"), Some("2023-01-01"));
}

#[tokio::test]
async fn max_tokens_comes_from_the_config() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let _ = complete_once(&server, |c| c.max_tokens = 123).await;
    assert_eq!(server.requests()[0].json()["max_tokens"], 123);
}

#[tokio::test]
async fn the_system_prompt_and_tools_are_sent_in_their_anthropic_shape() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let model = model(&server);
    let request = ModelRequest {
        system_prompt: Some("Be brief.".into()),
        messages: vec![Message::user("hi")],
        tools: vec![ToolSpec {
            name: "add".into(),
            description: "Adds.".into(),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Shared,
        }],
        settings: Value::Null,
    };
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    model.complete(request, cx).await.unwrap();

    let body = server.requests()[0].json();
    assert_eq!(body["system"], "Be brief.");
    assert_eq!(
        body["tools"],
        json!([{"name": "add", "description": "Adds.", "input_schema": {"type": "object"}}])
    );
}

#[tokio::test]
async fn reserved_settings_are_refused() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let model = model(&server);
    for key in [
        "model",
        "messages",
        "max_tokens",
        "stream",
        "system",
        "tools",
    ] {
        let request = ModelRequest {
            system_prompt: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            settings: json!({ key: 1 }),
        };
        let cx = ModelContext {
            cancellation: CancellationToken::new(),
            events: &NoopSink,
        };
        let error = model.complete(request, cx).await.unwrap_err();
        assert!(
            matches!(error, ModelError::Protocol(_)),
            "{key} must be refused: {error}"
        );
    }
    assert!(server.requests().is_empty(), "no request should be sent");
}

// ---------- the stream ----------

#[tokio::test]
async fn a_response_split_into_tiny_pieces_is_read_the_same() {
    let events = [
        message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        text_start(0),
        text_delta(0, "Hello, "),
        text_delta(0, "world"),
        block_stop(0),
        ping(),
        message_delta("end_turn", Some(json!({"output_tokens": 4}))),
        message_stop(),
    ];
    let server = MockServer::start(vec![Reply::sse_split(&events, 3)]).await;
    let response = complete_once(&server, |_| {}).await;
    assert_eq!(response.content, vec![ContentPart::text("Hello, world")]);
    assert_eq!(response.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn a_tool_call_round_trips_through_the_core() {
    let server = MockServer::start(vec![
        tool_reply("toolu_1", "add", "{\"a\":2,\"b\":3}"),
        text_reply("It is 5."),
    ])
    .await;
    let mut session = chat(model(&server), Calc).await;

    let outcome = session
        .run_turn("what is 2+3?", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_message.unwrap().text(), "It is 5.");
    let transcript = session.history_for_resume().history;
    assert_eq!(tool_result_text(&transcript, "toolu_1"), "5");

    // The second request sends the tool result back as a user message with a tool_result block.
    let second = server.requests()[1].json();
    assert_eq!(
        second["messages"].as_array().unwrap().last().unwrap(),
        &json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "5"}
        ]})
    );
}

#[tokio::test]
async fn a_server_that_reuses_tool_call_ids_does_not_break_the_conversation() {
    let server = MockServer::start(vec![
        tool_reply("toolu_dup", "add", "{\"a\":1,\"b\":1}"),
        text_reply("first done"),
        tool_reply("toolu_dup", "add", "{\"a\":2,\"b\":2}"),
        text_reply("second done"),
    ])
    .await;
    let mut session = chat(model(&server), Calc).await;

    session
        .run_turn("first", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();
    let outcome = session
        .run_turn("second", TurnOptions::default(), &NoopSink)
        .await
        .unwrap();

    assert_eq!(outcome.status, RunStatus::Completed);
    let ids: Vec<String> = session
        .history_for_resume()
        .history
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => {
                Some(tool_calls.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1], "a repeated provider id must be renamed");
}

#[tokio::test]
async fn thinking_and_server_tool_blocks_are_not_content() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        json!({"type": "content_block_start", "index": 0,
               "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "thinking_delta", "thinking": "internal reasoning"}}),
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "signature_delta", "signature": "EqQB"}}),
        block_stop(0),
        json!({"type": "content_block_start", "index": 1,
               "content_block": {"type": "server_tool_use", "id": "srvtoolu_1",
                                 "name": "web_search", "input": {}}}),
        block_stop(1),
        text_start(2),
        text_delta(2, "the answer"),
        block_stop(2),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let response = complete_once(&server, |_| {}).await;
    assert_eq!(response.content, vec![ContentPart::text("the answer")]);
    assert!(response.tool_calls.is_empty());
    // The thinking block is preserved for the next request, not exposed as content.
    let payload = response
        .provider_payload
        .expect("the thinking block is kept");
    let payload: Value = serde_json::from_str(payload.get()).unwrap();
    assert_eq!(
        payload,
        json!([{"type": "thinking", "thinking": "internal reasoning", "signature": "EqQB"}])
    );
}

/// Acceptance 6 and 8: a thinking-plus-tool_use turn sends the `thinking` object, keeps the
/// block out of `reasoning` and the UI events, and echoes it back (signature intact) before
/// the `tool_use` on the next request.
#[tokio::test]
async fn a_thinking_turn_echoes_the_blocks_back_on_the_next_request() {
    let server = MockServer::start(vec![
        Reply::sse(&[
            message_start(None),
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "thinking_delta", "thinking": "I should add."}}),
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "signature_delta", "signature": "EqQBsig"}}),
            block_stop(0),
            tool_start(1, Some("toolu_1"), "add", json!({})),
            input_json_delta(1, "{\"a\":2,\"b\":3}"),
            block_stop(1),
            message_delta("tool_use", None),
            message_stop(),
        ]),
        text_reply("It is 5."),
    ])
    .await;
    let recorder = Recorder::default();
    let mut session = chat(
        model_with(&server, |c| {
            c.thinking = Some(ThinkingConfig::Enabled {
                budget_tokens: 1024,
            });
        }),
        Calc,
    )
    .await;

    let outcome = session
        .run_turn("what is 2+3?", TurnOptions::default(), &recorder)
        .await
        .unwrap();
    assert_eq!(outcome.status, RunStatus::Completed);

    // The first request carries the configured `thinking` object.
    assert_eq!(
        server.requests()[0].json()["thinking"],
        json!({"type": "enabled", "budget_tokens": 1024})
    );
    // The second request echoes the assistant turn with the thinking block first.
    let second = server.requests()[1].json();
    assert_eq!(
        second["messages"][1],
        json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "I should add.", "signature": "EqQBsig"},
                {"type": "tool_use", "id": "toolu_1", "name": "add", "input": {"a": 2, "b": 3}}
            ]
        })
    );
    // Acceptance 8: thinking text is not in `reasoning` and emits no UI event.
    let history = session.history_for_resume().history;
    for message in &history {
        if let Message::Assistant { reasoning, .. } = message {
            assert!(
                reasoning.is_empty(),
                "thinking must not enter reasoning: {message:?}"
            );
        }
    }
    assert!(
        !recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::ModelReasoningDelta { .. })),
        "thinking must not reach the UI as reasoning"
    );
}

#[tokio::test]
async fn stop_reason_values_map_through_the_wire() {
    for (raw, expected) in [
        ("end_turn", StopReason::EndTurn),
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxTokens),
        ("pause_turn", StopReason::PauseTurn),
        ("refusal", StopReason::Refusal),
        ("stop_sequence", StopReason::EndTurn),
        ("model_context_window_exceeded", StopReason::MaxTokens),
        (
            "a_future_reason",
            StopReason::Other("a_future_reason".into()),
        ),
    ] {
        let server = MockServer::start(vec![Reply::sse(&[
            message_start(None),
            text_start(0),
            text_delta(0, "x"),
            block_stop(0),
            message_delta(raw, None),
            message_stop(),
        ])])
        .await;
        let response = complete_once(&server, |_| {}).await;
        assert_eq!(response.stop_reason, expected, "{raw}");
    }
}

#[tokio::test]
async fn a_stream_that_ends_before_finishing_is_an_error() {
    // The chunked body ends cleanly, but no `message_stop` ever arrives: the assembler must
    // refuse to return a partial answer.
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, "half an ans"),
    ])])
    .await;
    let model = model(&server);
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("before the response was complete")),
        "{error}"
    );
}

#[tokio::test]
async fn a_message_stop_without_a_stop_reason_is_an_error() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, "x"),
        block_stop(0),
        message_stop(),
    ])])
    .await;
    let model = model(&server);
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Transport(_)), "{error}");
}

#[tokio::test]
async fn a_silent_server_hits_the_idle_timeout() {
    let server = MockServer::start(vec![Reply::SseThenHang {
        pieces: vec![format!("data: {}\n\n", message_start(None)).into_bytes()],
    }])
    .await;
    let model = model_with(&server, |c| c.idle_timeout = Duration::from_millis(150));
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("sent nothing")),
        "{error}"
    );
}

#[tokio::test]
async fn cancelling_stops_a_request_that_is_waiting_on_the_server() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let model = model(&server);
    let token = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let cx = ModelContext {
            cancellation: token.clone(),
            events: &NoopSink,
        };
        tokio::join!(
            async {
                tokio::task::yield_now().await;
                token.cancel();
            },
            model.complete(
                ModelRequest {
                    system_prompt: None,
                    messages: vec![Message::user("hi")],
                    tools: Vec::new(),
                    settings: Value::Null,
                },
                cx,
            ),
        )
        .1
    })
    .await
    .expect("the cancelled call returns without the watchdog");
    assert!(
        matches!(&result, Err(ModelError::Transport(m)) if m.contains("cancelled")),
        "{result:?}"
    );
}

#[tokio::test]
async fn a_model_called_directly_honours_the_cancellation_token() {
    let server = MockServer::start(vec![text_reply("should not arrive")]).await;
    let model = model(&server);
    let token = CancellationToken::new();
    token.cancel();
    let cx = ModelContext {
        cancellation: token,
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Transport(_)), "{error}");
}

#[tokio::test]
async fn a_response_over_the_size_limit_is_refused() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, &"x".repeat(200)),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let model = model_with(&server, |c| c.max_response_bytes = 50);
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::Protocol(_)), "{error}");
}

// ---------- HTTP errors ----------

#[tokio::test]
async fn http_errors_are_classified_and_never_show_the_key() {
    // 401 is a request error; 500 and 529 are the provider's.
    for (code, transport) in [(401, false), (500, true), (529, true)] {
        let server = MockServer::start(vec![Reply::status(code, "nope")]).await;
        let model = model(&server);
        let cx = ModelContext {
            cancellation: CancellationToken::new(),
            events: &NoopSink,
        };
        let error = model
            .complete(
                ModelRequest {
                    system_prompt: None,
                    messages: vec![Message::user("hi")],
                    tools: Vec::new(),
                    settings: Value::Null,
                },
                cx,
            )
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&format!("HTTP {code}")), "{message}");
        assert!(message.contains("nope"), "{message}");
        assert!(!message.contains("sk-ant-secret-key"), "{message}");
        assert_eq!(
            matches!(error, ModelError::Transport(_)),
            transport,
            "{code}"
        );
    }
}

#[tokio::test]
async fn a_redirect_is_not_followed_so_the_key_stays_put() {
    let server = MockServer::start(vec![
        Reply::Status {
            code: 307,
            headers: vec![("location".into(), "/v1/messages".into())],
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
    let model = Anthropic::new(AnthropicConfig::new("http://127.0.0.1:1", "m")).unwrap();
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

#[tokio::test]
async fn message_start_and_message_delta_usage_are_merged() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(Some(
            json!({"input_tokens": 11, "cache_read_input_tokens": 3, "output_tokens": 1}),
        )),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta(
            "end_turn",
            Some(json!({"output_tokens": 4, "cache_creation_input_tokens": 7})),
        ),
        message_stop(),
    ])])
    .await;
    let response = complete_once(&server, |_| {}).await;
    let usage = response.usage.expect("usage was reported");
    assert_eq!(usage.input, Some(11));
    assert_eq!(usage.output, Some(4));
    assert_eq!(usage.cache_read, Some(3));
    assert_eq!(usage.cache_write, Some(7));
    assert_eq!(usage.total, None);
}

#[tokio::test]
async fn usage_details_are_kept_verbatim() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(Some(
            json!({"input_tokens": 1, "output_tokens_details": {"thinking_tokens": 5}}),
        )),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let response = complete_once(&server, |_| {}).await;
    let details = response.usage.unwrap().details.expect("details are kept");
    let parsed: Value = serde_json::from_str(details.get()).unwrap();
    assert_eq!(parsed, json!({"thinking_tokens": 5}));
}

#[tokio::test]
async fn a_response_without_usage_reports_none() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    // `text_reply` does carry usage; script one without it instead.
    let _ = server;
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let response = complete_once(&server, |_| {}).await;
    assert_eq!(response.usage, None);
}

#[tokio::test]
async fn total_is_never_invented_when_the_provider_omits_it() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(Some(json!({"input_tokens": 7, "output_tokens": 2}))),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let response = complete_once(&server, |_| {}).await;
    let usage = response.usage.expect("usage was reported");
    assert_eq!(usage.input, Some(7));
    assert_eq!(usage.output, Some(2));
    assert_eq!(usage.total, None, "total must stay None");
}

// ---------- errors, secrets and config ----------

#[tokio::test]
async fn an_http_error_body_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::status(400, "bad key sk-ant-secret-key")]).await;
    let model = model(&server);
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("sk-ant-secret-key"), "{error}");
    assert!(error.to_string().contains("HTTP 400"), "{error}");
}

#[tokio::test]
async fn a_stream_error_that_echoes_the_key_is_masked() {
    let server = MockServer::start(vec![Reply::sse(&[
        message_start(None),
        error("rejected key sk-ant-secret-key"),
    ])])
    .await;
    let model = model(&server);
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("sk-ant-secret-key"), "{error}");
    assert!(error.to_string().contains("rejected key"), "{error}");
}

#[tokio::test]
async fn a_custom_header_value_echoed_by_the_server_is_masked() {
    let server = MockServer::start(vec![Reply::status(500, "team-token-123 rejected")]).await;
    let model = model_with(&server, |c| {
        c.headers
            .push(("x-team-token".into(), "team-token-123".into()));
    });
    let cx = ModelContext {
        cancellation: CancellationToken::new(),
        events: &NoopSink,
    };
    let error = model
        .complete(
            ModelRequest {
                system_prompt: None,
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                settings: Value::Null,
            },
            cx,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("team-token-123"), "{error}");
}

#[tokio::test]
async fn the_key_is_hidden_from_debug_output() {
    let model = Anthropic::new(
        AnthropicConfig::new("https://api.anthropic.com", "m")
            .with_api_key(ApiKey::new("sk-ant-super-secret")),
    )
    .unwrap();
    let debug = format!("{model:?}");
    assert!(!debug.contains("sk-ant-super-secret"), "{debug}");
    assert!(debug.contains("ApiKey(<redacted>)"), "{debug}");
}

#[tokio::test]
async fn debug_output_hides_the_key_and_header_values() {
    let config = AnthropicConfig::new("https://api.anthropic.com", "m")
        .with_api_key(ApiKey::new("sk-ant-super-secret"));
    let mut config = config;
    config
        .headers
        .push(("x-secret-header".into(), "header-secret-value".into()));
    let debug = format!("{config:?}");
    assert!(!debug.contains("sk-ant-super-secret"), "{debug}");
    assert!(!debug.contains("header-secret-value"), "{debug}");
    assert!(debug.contains("x-secret-header"), "{debug}");
}

#[tokio::test]
async fn urls_never_leak_credentials_through_debug_or_config_errors() {
    let config = AnthropicConfig::new("https://user:pass@example.com/v1?key=secret", "m");
    let debug = format!("{config:?}");
    assert!(!debug.contains("user:pass"), "{debug}");
    assert!(!debug.contains("secret"), "{debug}");

    let error = Anthropic::new(AnthropicConfig::new("not a url", "m")).unwrap_err();
    assert!(!error.to_string().contains("not a url") || error.to_string().contains("invalid"));
    assert!(matches!(error, ConfigError::BaseUrl { .. }), "{error}");

    let error = Anthropic::new(AnthropicConfig::new("ftp://example.com", "m")).unwrap_err();
    assert!(matches!(error, ConfigError::BaseUrl { .. }), "{error}");
}

#[tokio::test]
async fn a_key_is_never_sent_over_plain_http_to_a_remote_host() {
    let error = Anthropic::new(
        AnthropicConfig::new("http://api.example.com", "m")
            .with_api_key(ApiKey::new("sk-ant-secret")),
    )
    .unwrap_err();
    assert!(matches!(error, ConfigError::InsecureKey { .. }), "{error}");
    // A local address is fine over http (the tests rely on it).
    assert!(
        Anthropic::new(
            AnthropicConfig::new("http://127.0.0.1:9", "m").with_api_key(ApiKey::new("k"))
        )
        .is_ok()
    );
}

#[tokio::test]
async fn bad_configuration_is_reported() {
    for url in ["http://", "https://", "://x"] {
        assert!(
            Anthropic::new(AnthropicConfig::new(url, "m")).is_err(),
            "{url}"
        );
    }
}
