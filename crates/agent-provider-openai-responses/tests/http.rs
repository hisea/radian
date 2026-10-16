//! Wire-level tests for the Responses adapter: the request it sends, the SSE it reads (split
//! and per event), error classification, secret masking and the rate-limit headers. The
//! semantic contract is `tests/contract.rs`; the per-event units are in `src/stream.rs`.

use std::time::Duration;

use agent_core::{
    CancellationToken, ContentPart, Message, MessageMeta, Model, ModelContext, ModelError,
    ModelRequest, ModelResponse, NoopSink, OpaqueJson, StopReason, ToolConcurrency, ToolSpec,
};
use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, function_call_added, function_call_arguments_delta,
    function_call_arguments_done, message_item, output_item_done, reasoning_item,
    reasoning_summary_delta, text_delta,
};
use agent_provider_openai_responses::{OpenAiResponses, OpenAiResponsesConfig};
use serde_json::{Value, json};

const SECRET: &str = "sk-contract-secret-token";

fn model(base: String) -> OpenAiResponses {
    let config = OpenAiResponsesConfig::new(base, "gpt-5.2-codex")
        .with_header("Authorization", format!("Bearer {SECRET}"))
        .with_header("chatgpt-account-id", "acct-1")
        .with_header("OpenAI-Beta", "responses=experimental")
        .with_header("originator", "radian");
    OpenAiResponses::new(config).expect("the mock config is valid")
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages,
        tools: Vec::new(),
        settings: Value::Null,
    }
}

async fn complete(
    model: &OpenAiResponses,
    request: ModelRequest,
) -> Result<ModelResponse, ModelError> {
    model
        .complete(
            request,
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
}

#[tokio::test]
async fn the_request_body_and_headers_are_the_responses_shape() {
    let server = MockServer::start(vec![Reply::sse(&[
        output_item_done(0, message_item("msg_1", "ok")),
        completed(None),
    ])])
    .await;
    let model = model(server.base_url.clone());
    let mut req = request(vec![Message::user("hello")]);
    req.system_prompt = Some("Be brief.".into());
    req.tools = vec![ToolSpec {
        name: "echo".into(),
        description: "Echoes.".into(),
        input_schema: json!({"type": "object"}),
        concurrency: ToolConcurrency::Shared,
    }];
    complete(&model, req).await.unwrap();

    let recorded = &server.requests()[0];
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/responses");
    assert_eq!(
        recorded.header("authorization"),
        Some(format!("Bearer {SECRET}").as_str())
    );
    assert_eq!(recorded.header("chatgpt-account-id"), Some("acct-1"));
    assert_eq!(
        recorded.header("openai-beta"),
        Some("responses=experimental")
    );
    assert_eq!(recorded.header("originator"), Some("radian"));

    let body = recorded.json();
    assert_eq!(body["model"], "gpt-5.2-codex");
    assert_eq!(body["store"], false, "store must be false");
    assert_eq!(body["stream"], true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["instructions"], "Be brief.");
    assert_eq!(
        body["input"],
        json!([{"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]}])
    );
    assert_eq!(
        body["tools"],
        json!([{"type": "function", "name": "echo", "description": "Echoes.",
                "parameters": {"type": "object"}}])
    );
}

#[tokio::test]
async fn an_encrypted_reasoning_item_round_trips_into_the_next_request() {
    let item = reasoning_item("rs_1", "the summary", "OPAQUE-ENCRYPTED-BLOB");
    let server = MockServer::start(vec![
        // First turn: a reasoning item, a visible answer, done.
        Reply::sse(&[
            json!({"type": "response.output_item.added", "output_index": 0, "item": item.clone()}),
            reasoning_summary_delta(0, "the summary"),
            output_item_done(0, item.clone()),
            output_item_done(0, message_item("msg_1", "the answer")),
            completed(None),
        ]),
        // Second turn: any valid response.
        Reply::sse(&[completed(None)]),
    ])
    .await;
    let model = model(server.base_url.clone());

    let first = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap();
    assert_eq!(first.reasoning, vec![ContentPart::text("the summary")]);
    let payload = first
        .provider_payload
        .clone()
        .expect("the reasoning item is kept for replay");

    // The next turn carries the previous assistant message back, payload and all.
    let history = vec![
        Message::user("hi"),
        Message::Assistant {
            content: first.content.clone(),
            reasoning: first.reasoning.clone(),
            tool_calls: Vec::new(),
            provider_payload: Some(payload),
            interrupted: false,
            meta: MessageMeta::default(),
        },
        Message::user("continue"),
    ];
    complete(&model, request(history)).await.unwrap();

    let body = server.requests()[1].json();
    let input = body["input"].as_array().expect("input is an array");
    assert_eq!(
        input[1], item,
        "the reasoning item is replayed verbatim, with its encrypted_content"
    );
    assert_eq!(input[1]["encrypted_content"], "OPAQUE-ENCRYPTED-BLOB");
    assert_eq!(input[2]["type"], "message");
    assert_eq!(input[2]["role"], "assistant");
}

#[tokio::test]
async fn tool_call_arguments_split_across_deltas_rejoin_over_the_wire() {
    let server = MockServer::start(vec![Reply::sse(&[
        function_call_added(0, "fc_1", "c1", "calc"),
        function_call_arguments_delta(0, "fc_1", "{\"a\":"),
        function_call_arguments_delta(0, "fc_1", "2, \"b\":"),
        function_call_arguments_delta(0, "fc_1", "3}"),
        completed(None),
    ])])
    .await;
    let model = model(server.base_url.clone());
    let response = complete(&model, request(vec![Message::user("add")]))
        .await
        .unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "c1");
    assert_eq!(response.tool_calls[0].arguments, json!({"a": 2, "b": 3}));
}

#[tokio::test]
async fn the_authoritative_arguments_done_wins_over_the_deltas() {
    let server = MockServer::start(vec![Reply::sse(&[
        function_call_added(0, "fc_1", "c1", "calc"),
        function_call_arguments_delta(0, "fc_1", "{\"a\": 9"),
        function_call_arguments_done(0, "fc_1", "{\"a\": 1}"),
        completed(None),
    ])])
    .await;
    let model = model(server.base_url.clone());
    let response = complete(&model, request(vec![Message::user("add")]))
        .await
        .unwrap();
    assert_eq!(response.tool_calls[0].arguments, json!({"a": 1}));
}

#[tokio::test]
async fn a_stream_split_into_tiny_pieces_reads_the_same() {
    let events = [
        output_item_done(0, message_item("msg_1", "Hello, world!")),
        completed(None),
    ];
    let server = MockServer::start(vec![Reply::sse_split(&events, 1)]).await;
    let model = model(server.base_url.clone());
    let response = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap();
    assert_eq!(response.content, vec![ContentPart::text("Hello, world!")]);
}

#[tokio::test]
async fn the_codex_rate_limit_headers_are_kept_raw_on_the_response() {
    let server = MockServer::start(vec![Reply::sse_with_headers(
        &[
            output_item_done(0, message_item("msg_1", "ok")),
            completed(None),
        ],
        vec![
            ("x-codex-primary-used-percent".into(), "12.5".into()),
            ("x-codex-secondary-window-minutes".into(), "10080".into()),
            ("x-unrelated".into(), "ignored".into()),
        ],
    )])
    .await;
    let model = model(server.base_url.clone());
    let response = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap();
    let limits = response.limits.expect("the x-codex headers are kept");
    assert_eq!(
        limits.headers,
        vec![
            (
                "x-codex-primary-used-percent".to_string(),
                "12.5".to_string()
            ),
            (
                "x-codex-secondary-window-minutes".to_string(),
                "10080".to_string()
            ),
        ]
    );
}

#[tokio::test]
async fn a_response_without_rate_limit_headers_has_no_limits() {
    let server = MockServer::start(vec![Reply::sse(&[completed(None)])]).await;
    let model = model(server.base_url.clone());
    let response = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap();
    assert!(response.limits.is_none());
}

#[tokio::test]
async fn a_429_is_transient_and_carries_the_retry_after() {
    let server = MockServer::start(vec![Reply::status_with_headers(
        429,
        "slow down",
        vec![("Retry-After".into(), "2".into())],
    )])
    .await;
    let model = model(server.base_url.clone());
    let error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ModelError::Unavailable { message, retry_after }
            if message.contains("HTTP 429") && message.contains("slow down")
                && *retry_after == Some(Duration::from_secs(2))),
        "{error}"
    );
    assert!(error.is_transient());
}

#[tokio::test]
async fn a_500_is_transient_and_a_400_is_not() {
    let server = MockServer::start(vec![
        Reply::status(500, "oops"),
        Reply::status(400, "bad request"),
    ])
    .await;
    let model = model(server.base_url.clone());
    let server_error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&server_error, ModelError::Unavailable { message, .. }
            if message.contains("HTTP 500")),
        "{server_error}"
    );
    assert!(server_error.is_transient());

    let request_error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&request_error, ModelError::Protocol(m) if m.contains("HTTP 400")),
        "{request_error}"
    );
    assert!(!request_error.is_transient());
}

#[tokio::test]
async fn a_recognizable_context_overflow_is_its_own_error() {
    let server = MockServer::start(vec![Reply::status(
        400,
        &json!({"error": {
            "message": "This model's maximum context length is 16385 tokens.",
            "code": "context_length_exceeded"
        }})
        .to_string(),
    )])
    .await;
    let model = model(server.base_url.clone());
    let error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert!(matches!(error, ModelError::ContextOverflow(_)), "{error}");
}

#[tokio::test]
async fn an_error_body_that_echoes_the_token_is_masked() {
    let server = MockServer::start(vec![Reply::status(
        400,
        &format!("bad request: rejected key {SECRET}"),
    )])
    .await;
    let model = model(server.base_url.clone());
    let error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(!message.contains(SECRET), "{message}");
    assert!(message.contains("<redacted>"), "{message}");
}

#[tokio::test]
async fn a_stream_that_goes_silent_is_a_transport_error() {
    let server = MockServer::start(vec![Reply::SseThenHang {
        pieces: vec![format!("data: {}\n\n", text_delta(0, "half")).into_bytes()],
    }])
    .await;
    let mut config = OpenAiResponsesConfig::new(server.base_url.clone(), "gpt-5.2-codex")
        .with_header("Authorization", format!("Bearer {SECRET}"));
    config.idle_timeout = Duration::from_millis(50);
    let model = OpenAiResponses::new(config).unwrap();
    let error = complete(&model, request(vec![Message::user("hi")]))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("nothing")),
        "{error}"
    );
}

#[tokio::test]
async fn cancelling_in_flight_returns_a_cancelled_transport_error() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let model = model(server.base_url.clone());
    let token = CancellationToken::new();
    let ctx_token = token.clone();
    let result = tokio::join!(
        async {
            tokio::task::yield_now().await;
            token.cancel();
        },
        model.complete(
            request(vec![Message::user("hi")]),
            ModelContext {
                cancellation: ctx_token,
                events: &NoopSink,
            }
        ),
    )
    .1;
    assert!(
        matches!(&result, Err(ModelError::Transport(m)) if m.contains("cancelled")),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_interrupted_assistant_is_sent_as_a_plain_assistant() {
    // Same content, only the flag differs: the wire bodies must be byte-identical (the core's
    // interrupted prefix is not a wire concept).
    let server = MockServer::start(vec![
        Reply::sse(&[completed(None)]),
        Reply::sse(&[completed(None)]),
    ])
    .await;
    let model = model(server.base_url.clone());
    let plain = request(vec![Message::assistant("Let me think...")]);
    complete(&model, plain).await.unwrap();
    let interrupted = request(vec![Message::Assistant {
        content: vec![ContentPart::text("Let me think...")],
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        provider_payload: None,
        interrupted: true,
        meta: MessageMeta::default(),
    }]);
    complete(&model, interrupted).await.unwrap();
    let bodies: Vec<String> = server.requests().iter().map(|r| r.body.clone()).collect();
    assert_eq!(bodies[0], bodies[1]);
    assert!(!bodies[1].contains("interrupted"));
}

#[tokio::test]
async fn a_foreign_provider_payload_does_not_leak_into_the_request() {
    let server = MockServer::start(vec![Reply::sse(&[completed(None)])]).await;
    let model = model(server.base_url.clone());
    let history = vec![Message::Assistant {
        content: vec![ContentPart::text("answer")],
        reasoning: vec![ContentPart::text("foreign reasoning")],
        tool_calls: Vec::new(),
        // Anthropic-shaped thinking payload.
        provider_payload: Some(
            OpaqueJson::parse(r#"[{"type":"thinking","thinking":"x","signature":"sig"}]"#).unwrap(),
        ),
        interrupted: false,
        meta: MessageMeta::default(),
    }];
    complete(&model, request(history)).await.unwrap();
    let body = server.requests()[0].body.clone();
    assert!(!body.contains("thinking"), "{body}");
    assert!(!body.contains("foreign reasoning"), "{body}");
}
