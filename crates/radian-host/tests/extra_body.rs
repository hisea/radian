//! `RADIAN_EXTRA_BODY` (ADR 0026 D13): the parsed map reaches every request builder — chat,
//! Responses and Anthropic — under the request's own settings, and the startup rules refuse a
//! non-object and a reserved key.
//!
//! The value is never read from the process environment here: `parse_extra_body` is the rule
//! the hosts call, and the end-to-end startup path is pinned in `radian-exec`'s CLI tests.

use agent_provider_anthropic::testing::{
    MockServer as AnthropicServer, Reply as AnthropicReply, block_stop, message_delta,
    message_start, message_stop, text_delta, text_start,
};
use agent_provider_openai_compatible::testing::{
    MockServer as ChatServer, Reply as ChatReply, chunk,
};
use agent_provider_openai_responses::testing::{
    MockServer as ResponsesServer, Reply as ResponsesReply, completed, message_item,
    output_item_done,
};
use radian_core::{
    CancellationToken, Message, Model, ModelContext, ModelRequest, ModelResponse, NoopSink,
};
use radian_host::{ExtraBody, ModelArgs, Provider, Thinking, build_model, parse_extra_body};
use serde_json::{Map, Value, json};

fn parsed(raw: &str) -> ExtraBody {
    parse_extra_body(Some(raw.to_owned()))
        .expect("the value parses")
        .expect("a non-empty object")
}

fn fields(extra: &ExtraBody) -> Map<String, Value> {
    extra.fields().clone()
}

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages,
        tools: Vec::new(),
        settings: Value::Null,
    }
}

async fn complete(model: &radian_host::HostModel, request: ModelRequest) -> ModelResponse {
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

fn chat_model(server: &ChatServer, extra_body: Map<String, Value>) -> radian_host::HostModel {
    build_model(ModelArgs {
        extra_body,
        provider: Provider::OpenAiCompatible,
        base_url: server.base_url.clone(),
        model: "mock".to_owned(),
        api_key: Some("sk-test".to_owned()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .expect("the chat model builds")
}

/// ADR 0026 D13: the value the brief measured (`thinking`) reaches an OpenAI-compatible chat
/// request body.
#[tokio::test]
async fn the_extra_body_reaches_the_chat_request_body() {
    let server = ChatServer::start(vec![ChatReply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let extra = parsed(r#"{"thinking":{"type":"enabled"}}"#);
    let model = chat_model(&server, fields(&extra));
    complete(&model, request(vec![Message::user("hi")])).await;

    let body = server.requests()[0].json();
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
}

/// Every route a `/model` switch can build carries the same map.
#[tokio::test]
async fn the_extra_body_reaches_the_responses_and_anthropic_request_bodies() {
    let responses = ResponsesServer::start(vec![ResponsesReply::sse(&[
        output_item_done(0, message_item("msg_1", "ok")),
        completed(None),
    ])])
    .await;
    let model = build_model(ModelArgs {
        extra_body: fields(&parsed(r#"{"thinking":{"type":"enabled"}}"#)),
        provider: Provider::ChatGpt,
        base_url: responses.base_url.clone(),
        model: "mock".to_owned(),
        api_key: Some("sk-test".to_owned()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .expect("the Responses model builds");
    complete(&model, request(vec![Message::user("hi")])).await;
    let body = responses.requests()[0].json();
    assert_eq!(body["thinking"], json!({"type": "enabled"}));

    let anthropic = AnthropicServer::start(vec![AnthropicReply::sse(&[
        message_start(None),
        text_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta("end_turn", None),
        message_stop(),
    ])])
    .await;
    let model = build_model(ModelArgs {
        extra_body: fields(&parsed(r#"{"temperature":0.2}"#)),
        provider: Provider::Anthropic,
        base_url: anthropic.base_url.clone(),
        model: "claude-test".to_owned(),
        api_key: Some("sk-ant-test".to_owned()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 2048,
        thinking: Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        max_output_tokens: None,
        chatgpt_oauth: false,
    })
    .expect("the Anthropic model builds");
    complete(&model, request(vec![Message::user("hi")])).await;
    let body = anthropic.requests()[0].json();
    assert_eq!(body["temperature"], json!(0.2));
}

/// The request's own settings win over the extra body, and the startup warning names the key.
#[tokio::test]
async fn a_request_setting_wins_over_the_extra_body_and_the_warning_names_the_key() {
    let server = ChatServer::start(vec![ChatReply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let extra = parsed(r#"{"temperature":0.1}"#);
    let settings = json!({"temperature": 0.9});
    let model = chat_model(&server, fields(&extra));
    let mut request = request(vec![Message::user("hi")]);
    request.settings = settings.clone();
    complete(&model, request).await;

    let body = server.requests()[0].json();
    assert_eq!(body["temperature"], json!(0.9), "the request value wins");
    let warning = extra
        .settings_warning(&settings)
        .expect("the overlap is said once");
    assert!(warning.contains("temperature"), "{warning}");
    assert!(
        !warning.contains("0.9"),
        "the request's value never appears: {warning}"
    );
}

/// ADR 0026 D13: reserved keys are checked per route, not as a union. A chat route accepts
/// `max_tokens` (its builder inserts the catalog cap only when the body has none); the
/// Anthropic route reserves it. A non-object is still a parser refusal.
#[test]
fn reserved_keys_are_checked_per_adapter_and_non_objects_are_refused() {
    let extra = parsed(r#"{"max_tokens":32000}"#);
    assert!(extra.refused_keys(Provider::OpenAiCompatible).is_empty());
    assert!(extra.refused_keys(Provider::ChatGpt).is_empty());
    assert_eq!(extra.refused_keys(Provider::Anthropic), vec!["max_tokens"]);
    let line = extra
        .refusal_for(Provider::Anthropic)
        .expect("the Anthropic route refuses the key");
    assert!(line.contains("RADIAN_EXTRA_BODY"), "{line}");
    assert!(line.contains("max_tokens"), "{line}");

    // `messages` is reserved by chat and Anthropic, but not by Responses.
    let extra = parsed(r#"{"messages":[]}"#);
    assert_eq!(
        extra.refused_keys(Provider::OpenAiCompatible),
        vec!["messages"]
    );
    assert_eq!(extra.refused_keys(Provider::Anthropic), vec!["messages"]);
    assert!(extra.refused_keys(Provider::ChatGpt).is_empty());

    let error = parse_extra_body(Some("[1]".to_owned())).expect_err("non-object");
    assert!(error.contains("RADIAN_EXTRA_BODY"), "{error}");
    assert!(error.contains("object"), "{error}");

    assert_eq!(parse_extra_body(None).expect("unset is fine"), None);
    assert_eq!(
        parse_extra_body(Some(String::new())).expect("empty is fine"),
        None
    );
}

/// ADR 0026 D13: the switch check resolves the target route's adapter and consults only that
/// adapter. A switch between two chat routes with the same body is allowed; a chat-to-Anthropic
/// switch with `max_tokens` is refused.
#[test]
fn a_switch_between_chat_routes_is_allowed_and_a_switch_to_anthropic_is_refused() {
    let extra = parsed(r#"{"max_tokens":32000}"#);
    // Two chat-resolving route names: the generic descriptor and a named provider that declares
    // `openai-compatible`. Both accept `max_tokens`; the Anthropic route refuses it.
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "second-chat".to_owned(),
        radian_host::ProviderDecl {
            kind: "openai-compatible".to_owned(),
            base_url: "http://127.0.0.1:9".to_owned(),
            reasoning_replay: None,
        },
    );
    let chat = radian_host::route_adapter_for("openai-compatible", "m", &providers)
        .expect("the generic descriptor resolves");
    assert_eq!(chat, Provider::OpenAiCompatible);
    let second_chat = radian_host::route_adapter_for("second-chat", "m", &providers)
        .expect("the named chat provider resolves");
    assert_eq!(second_chat, Provider::OpenAiCompatible);
    assert_eq!(extra.refusal_for(chat), None, "chat accepts max_tokens");
    assert_eq!(
        extra.refusal_for(second_chat),
        None,
        "a second chat route accepts max_tokens too"
    );

    let anthropic = radian_host::route_adapter_for("anthropic", "claude-x", &providers)
        .expect("the anthropic descriptor resolves");
    assert_eq!(anthropic, Provider::Anthropic);
    let line = extra
        .refusal_for(anthropic)
        .expect("the switch to Anthropic is refused");
    assert!(line.contains("RADIAN_EXTRA_BODY"), "{line}");
    assert!(line.contains("max_tokens"), "{line}");
}

/// ADR 0026 D13: an explicit `max_tokens` in the body wins over the catalog-derived cap, so
/// a chat route sends the user's number rather than overwriting it.
#[tokio::test]
async fn a_chat_route_sends_an_explicit_max_tokens_over_the_catalog_cap() {
    let server = ChatServer::start(vec![ChatReply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let model = build_model(ModelArgs {
        extra_body: fields(&parsed(r#"{"max_tokens":32000}"#)),
        provider: Provider::OpenAiCompatible,
        base_url: server.base_url.clone(),
        model: "mock".to_owned(),
        api_key: Some("sk-test".to_owned()),
        headers: Vec::new(),
        include_usage: None,
        max_tokens: 1024,
        thinking: Thinking::Off,
        reasoning_effort: None,
        reasoning_replay: None,
        // The catalog cap the adapter would insert when the body has none.
        max_output_tokens: Some(4096),
        chatgpt_oauth: false,
    })
    .expect("the chat model builds");
    complete(&model, request(vec![Message::user("hi")])).await;

    let body = server.requests()[0].json();
    assert_eq!(body["max_tokens"], json!(32000), "{body}");
}
