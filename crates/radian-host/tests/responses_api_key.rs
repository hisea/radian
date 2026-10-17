//! ADR 0026: a **built-in descriptor's** Responses model authenticated by
//! an API key. `build_model` must build a plain Responses adapter that sends
//! `Authorization: Bearer <key>` plus the descriptor's own headers, and none of the three
//! Codex-only headers (`chatgpt-account-id`, `OpenAI-Beta`, `originator`) — no OAuth refresh.
//! The `chatgpt` route's own headers are pinned by `tests/chatgpt_headers.rs`.
//!
//! Everything runs against a local mock; no test touches `chatgpt.com` or `opencode.ai`.

use agent_core::{
    CancellationToken, Message, Model, ModelContext, ModelRequest, NoopSink, StopReason,
};
use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, function_call_added, function_call_arguments_done, message_item,
    output_item_done, text_delta, usage,
};
use radian_host::{ModelArgs, Provider, Thinking, build_model};
use serde_json::Value;

/// The API-key Responses model exactly as `radian-host`'s switchable model builds it for a
/// `@ai-sdk/openai` built-in descriptor model (`opencode-go`'s headers, no `chatgpt_oauth`).
fn api_key_model(server: &MockServer) -> radian_host::HostModel {
    build_model(ModelArgs {
        provider: Provider::ChatGpt,
        base_url: server.base_url.clone(),
        model: "muse-spark-1.3-contributor".to_owned(),
        api_key: Some("sk-test-key".to_owned()),
        headers: vec![
            ("x-opencode-session".to_owned(), "s1".to_owned()),
            ("x-opencode-client".to_owned(), "radian".to_owned()),
        ],
        include_usage: None,
        max_tokens: 4096,
        thinking: Thinking::Off,
        reasoning_effort: Some("high".to_owned()),
        reasoning_replay: None,
        chatgpt_oauth: false,
    })
    .expect("the API-key Responses model builds")
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        system_prompt: Some("Be brief.".to_owned()),
        messages,
        tools: Vec::new(),
        settings: Value::Null,
    }
}

async fn complete(
    model: &radian_host::HostModel,
    request: ModelRequest,
) -> agent_core::ModelResponse {
    model
        .complete(
            request,
            ModelContext {
                cancellation: CancellationToken::new(),
                events: &NoopSink,
            },
        )
        .await
        .expect("the mock answers")
}

/// The whole credential: `Authorization: Bearer <key>`, the descriptor's headers, and **none**
/// of the three Codex headers. The request path is the Responses one (`/responses`), and the
/// body is the same Responses shape (with the effort the route resolved).
#[tokio::test]
async fn an_api_key_responses_route_sends_the_bearer_and_no_codex_headers() {
    let server = MockServer::start(vec![Reply::sse(&[
        output_item_done(0, message_item("msg_1", "ok")),
        completed(None),
    ])])
    .await;
    let model = api_key_model(&server);
    let response = complete(&model, request(vec![Message::user("hello")])).await;
    assert_eq!(response.stop_reason, StopReason::EndTurn);

    let recorded = &server.requests()[0];
    assert_eq!(recorded.method, "POST");
    assert_eq!(
        recorded.path, "/responses",
        "the Responses adapter appends `/responses` to the base"
    );
    assert_eq!(
        recorded.header("authorization"),
        Some("Bearer sk-test-key")
    );
    for name in ["chatgpt-account-id", "openai-beta", "originator"] {
        assert!(
            recorded.header(name).is_none(),
            "the API-key route must not send the Codex-only header `{name}`: {:?}",
            recorded.headers
        );
    }
    assert_eq!(recorded.header("x-opencode-session"), Some("s1"));
    assert_eq!(recorded.header("x-opencode-client"), Some("radian"));
    let body = recorded.json();
    assert_eq!(body["model"], "muse-spark-1.3-contributor");
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"], serde_json::json!({"effort": "high"}));
    assert_eq!(body["instructions"], "Be brief.");
    assert!(
        !recorded.body.contains("sk-test-key"),
        "the key is the Authorization header and nothing else"
    );
}

/// One end-to-end turn of each shape over the API-key route: a streamed text answer, then a turn
/// whose reply is a tool call (with its arguments split across deltas).
#[tokio::test]
async fn a_text_turn_and_a_tool_turn_run_over_the_api_key_responses_route() {
    let server = MockServer::start(vec![
        Reply::sse(&[
            text_delta(0, "Hel"),
            text_delta(0, "lo"),
            output_item_done(0, message_item("msg_1", "Hello")),
            completed(Some(usage(10, 2, 12, 0))),
        ]),
        Reply::sse(&[
            function_call_added(0, "fc_1", "call_1", "read"),
            function_call_arguments_done(0, "fc_1", r#"{"path":"x"}"#),
            completed(None),
        ]),
    ])
    .await;
    let model = api_key_model(&server);

    let text = complete(&model, request(vec![Message::user("say hi")])).await;
    assert_eq!(text.stop_reason, StopReason::EndTurn);
    assert_eq!(
        text.content,
        vec![agent_core::ContentPart::text("Hello")],
        "the streamed deltas assemble into the answer"
    );
    assert_eq!(text.usage.map(|usage| usage.input), Some(Some(10)));

    let tool = complete(&model, request(vec![Message::user("read x")])).await;
    assert_eq!(tool.stop_reason, StopReason::ToolUse);
    assert_eq!(tool.tool_calls.len(), 1);
    assert_eq!(tool.tool_calls[0].name, "read");
    assert_eq!(tool.tool_calls[0].arguments, serde_json::json!({"path": "x"}));

    // Every request went the same way: the Bearer key and no Codex header.
    for recorded in server.requests() {
        assert_eq!(
            recorded.header("authorization"),
            Some("Bearer sk-test-key")
        );
        assert!(recorded.header("originator").is_none());
        assert!(recorded.header("openai-beta").is_none());
        assert!(recorded.header("chatgpt-account-id").is_none());
    }
}
