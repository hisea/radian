//! `ModelRequest` -> Anthropic Messages request body. Pure, so it is tested without a server.

use agent_core::{
    ContentPart, Message, ModelError, ModelRequest, OpaqueJson, ToolCall, ToolResult,
};
use serde_json::{Map, Value, json};

use crate::config::{AnthropicConfig, CacheMode, ThinkingConfig};

/// Keys the adapter owns. Settings may not override them. `max_tokens` is here because
/// Anthropic requires it and the adapter always sends it (see the wire notes §2).
const RESERVED: [&str; 7] = [
    "model",
    "messages",
    "max_tokens",
    "stream",
    "system",
    "tools",
    "tool_choice",
];

/// The documented maximum number of explicit `cache_control` breakpoints in one request
/// (wire notes §2.4). The adapter places at most three (last tool, end of system, last
/// cacheable block of the last message), so it never approaches this; `build_body` asserts it.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// The only cache type Anthropic supports today: a 5-minute ephemeral entry.
fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

pub(crate) fn build_body(
    request: &ModelRequest,
    config: &AnthropicConfig,
) -> Result<Value, ModelError> {
    let mut body = Map::new();
    for source in [&config.extra_body, &settings_object(&request.settings)?] {
        for (key, value) in source {
            if RESERVED.contains(&key.as_str()) {
                return Err(ModelError::Protocol(format!(
                    "the request setting `{key}` is set by the adapter and cannot be overridden"
                )));
            }
            body.insert(key.clone(), value.clone());
        }
    }
    body.insert("model".into(), json!(config.model));
    body.insert("max_tokens".into(), json!(config.max_tokens));
    body.insert("stream".into(), json!(true));
    // The config wins over any `thinking` passed through `extra_body`/`settings`, but when no
    // thinking is configured the key is simply absent, so a thinking-free request is
    // byte-identical to the adapter before this feature (acceptance 5).
    if let Some(thinking) = config.thinking {
        body.insert(
            "thinking".into(),
            match thinking {
                ThinkingConfig::Enabled { budget_tokens } => {
                    json!({"type": "enabled", "budget_tokens": budget_tokens})
                }
                ThinkingConfig::Adaptive => json!({"type": "adaptive"}),
            },
        );
    }
    let caching = config.cache == CacheMode::Auto;
    // Count the breakpoints the adapter itself places. The documented limit governs these
    // explicit marks, so a key that merely shares the name inside a tool's `input_schema` or a
    // tool call's arguments is the caller's data and must not be mistaken for a breakpoint.
    let mut breakpoints = 0usize;
    if let Some(system) = &request.system_prompt {
        let marked = caching && !system.is_empty();
        breakpoints += usize::from(marked);
        body.insert("system".into(), system_value(system, marked));
    }
    let mut messages = messages(&request.messages)?;
    if caching && cache_last_message_block(&mut messages) {
        breakpoints += 1;
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        if caching {
            // The last tool definition is the tool breakpoint; tools earlier in the array stay
            // as they are (their prefix is covered by the breakpoint's cumulative hash).
            if let Some(last) = tools.last_mut().and_then(Value::as_object_mut) {
                last.insert("cache_control".into(), cache_control());
                breakpoints += 1;
            }
        }
        body.insert("tools".into(), Value::Array(tools));
    }
    let body = Value::Object(body);
    debug_assert!(breakpoints <= MAX_CACHE_BREAKPOINTS);
    Ok(body)
}

/// The `system` field. When [`mark_breakpoint`] is set (caching on and the prompt non-empty), it
/// becomes the documented array form with the breakpoint on its last (only) block. Otherwise it
/// stays the plain string it is today: `text` has `minLength 1`, so an empty block may not exist,
/// let alone carry `cache_control` (wire notes §2.4).
fn system_value(system: &str, mark_breakpoint: bool) -> Value {
    if mark_breakpoint {
        json!([{
            "type": "text",
            "text": system,
            "cache_control": cache_control(),
        }])
    } else {
        json!(system)
    }
}

/// Put one `cache_control` breakpoint on the last cacheable block of the last message, in
/// place. Thinking and redacted_thinking blocks cannot be cached directly, and an empty block
/// cannot either, so the scan walks back to the last eligible block. A final message whose
/// content is a plain string (the empty-user fallback) has no block to mark. Returns whether a
/// breakpoint was placed.
fn cache_last_message_block(messages: &mut [Value]) -> bool {
    let Some(blocks) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return false;
    };
    for block in blocks.iter_mut().rev() {
        if is_cacheable(block) {
            if let Some(map) = block.as_object_mut() {
                map.insert("cache_control".into(), cache_control());
            }
            return true;
        }
    }
    false
}

/// Whether a content block may carry `cache_control`: text, image, tool_use and tool_result
/// can, thinking and redacted_thinking cannot, and an empty text block cannot (wire notes
/// §2.4). The adapter builds no other block type.
fn is_cacheable(block: &Value) -> bool {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().is_some_and(|text| !text.is_empty()),
        Some("thinking" | "redacted_thinking") => false,
        Some(_) => true,
        None => false,
    }
}

/// How many `cache_control` keys a value carries, at any depth. The tests use it as an oracle
/// over bodies they build themselves, where every occurrence is a real breakpoint; `build_body`
/// checks its own documented limit with a counter instead, so a caller's lookalike key (see
/// `auto_does_not_mistake_a_user_data_key_for_a_breakpoint`) cannot trip it. Test-only, so it is
/// not compiled into the shipped adapter.
#[cfg(test)]
fn count_cache_breakpoints(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            map.keys()
                .filter(|key| key.as_str() == "cache_control")
                .count()
                + map.values().map(count_cache_breakpoints).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_cache_breakpoints).sum(),
        _ => 0,
    }
}

fn settings_object(settings: &Value) -> Result<Map<String, Value>, ModelError> {
    match settings {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(ModelError::Protocol(
            "model settings must be a JSON object".into(),
        )),
    }
}

/// Anthropic has no `tool` role: tool results are `tool_result` blocks inside a `user`
/// message. Consecutive results are merged into one user message, matching the official
/// round-trip example (wire notes §2.1).
fn messages(messages: &[Message]) -> Result<Vec<Value>, ModelError> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < messages.len() {
        match &messages[index] {
            Message::User { content, .. } => {
                out.push(user_message(content));
                index += 1;
            }
            Message::Assistant {
                content,
                tool_calls,
                provider_payload,
                ..
            } => {
                out.push(assistant_message(
                    content,
                    tool_calls,
                    provider_payload.as_ref(),
                )?);
                index += 1;
            }
            Message::Tool { .. } => {
                let mut blocks = Vec::new();
                while let Some(Message::Tool { result, .. }) = messages.get(index) {
                    blocks.push(tool_result_block(result));
                    index += 1;
                }
                out.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    Ok(out)
}

fn user_message(content: &[ContentPart]) -> Value {
    let blocks = blocks(content);
    // An empty content is not a documented shape; a plain empty string is the least
    // surprising fallback (and cannot happen for messages the core builds in practice).
    let content = if blocks.is_empty() {
        json!("")
    } else {
        Value::Array(blocks)
    };
    json!({"role": "user", "content": content})
}

fn assistant_message(
    content: &[ContentPart],
    tool_calls: &[ToolCall],
    provider_payload: Option<&OpaqueJson>,
) -> Result<Value, ModelError> {
    // Anthropic requires the thinking/redacted_thinking blocks of the assistant turn to be
    // echoed back complete and unmodified, before the tool_use they accompanied; the payload
    // holds exactly those blocks, in the order they were streamed (ADR 0014 R10; wire notes
    // §4.5).
    let mut preserved = thinking_blocks(provider_payload)?;
    preserved.extend(blocks(content));
    preserved.extend(tool_calls.iter().map(tool_use_block));
    Ok(json!({"role": "assistant", "content": preserved}))
}

/// The preserved `thinking`/`redacted_thinking` blocks from an assistant message's
/// `provider_payload`, in order. Only those two block types are returned: the rest of an
/// assistant message is rebuilt from `content` and `tool_calls`, so re-inserting an opaque
/// block of another type would duplicate or misplace it. A payload that is not a JSON array
/// is a protocol error rather than a silent drop (a dropped thinking block makes the
/// provider reject the request).
fn thinking_blocks(payload: Option<&OpaqueJson>) -> Result<Vec<Value>, ModelError> {
    let Some(payload) = payload else {
        return Ok(Vec::new());
    };
    let value: Value = serde_json::from_str(payload.get()).map_err(|error| {
        ModelError::Protocol(format!("provider_payload is not valid JSON: {error}"))
    })?;
    let Value::Array(blocks) = value else {
        return Err(ModelError::Protocol(
            "provider_payload must be a JSON array of content blocks".into(),
        ));
    };
    Ok(blocks
        .into_iter()
        .filter(|block| {
            matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        })
        .collect())
}

/// Text and image blocks; every other `ContentPart` becomes a short text placeholder, so the
/// model knows something was there. Empty text blocks are dropped (`text` has minLength 1).
fn blocks(parts: &[ContentPart]) -> Vec<Value> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } if text.is_empty() => None,
            other => Some(block(other)),
        })
        .collect()
}

fn block(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text, .. } => json!({"type": "text", "text": text}),
        ContentPart::Image {
            mime_type, data, ..
        } => json!({
            "type": "image",
            "source": {"type": "base64", "media_type": mime_type, "data": data}
        }),
        other => json!({"type": "text", "text": text_of(other)}),
    }
}

fn tool_use_block(call: &ToolCall) -> Value {
    json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.name,
        "input": input_object(&call.arguments),
    })
}

/// Anthropic's `tool_use.input` is always an object. A string that parses to an object is
/// used; anything else becomes `{}` (the adapter does not invent structure for arguments the
/// model produced badly). This is lossy and recorded in the wire notes.
fn input_object(arguments: &Value) -> Value {
    match arguments {
        Value::Object(_) => arguments.clone(),
        Value::String(raw) => serde_json::from_str::<Value>(raw)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}

fn tool_result_block(result: &ToolResult) -> Value {
    let text = plain_text(&result.content);
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    block.insert("tool_use_id".into(), json!(result.call_id));
    if !text.is_empty() {
        block.insert("content".into(), json!(text));
    }
    if result.is_error {
        block.insert("is_error".into(), json!(true));
    }
    Value::Object(block)
}

/// Text of every part, joined by blank lines. Parts the API cannot take as text become short
/// placeholders. This is lossy by design.
fn plain_text(parts: &[ContentPart]) -> String {
    parts.iter().map(text_of).collect::<Vec<_>>().join("\n\n")
}

fn text_of(part: &ContentPart) -> String {
    match part {
        ContentPart::Text { text, .. } => text.clone(),
        ContentPart::Image { mime_type, .. } => format!("[{mime_type} image omitted]"),
        ContentPart::Audio { mime_type, .. } => format!("[{mime_type} audio omitted]"),
        ContentPart::ResourceLink { uri, name, .. } => match name {
            Some(name) => format!("[resource {name}: {uri}]"),
            None => format!("[resource: {uri}]"),
        },
        ContentPart::EmbeddedResource { resource, .. } => match resource["text"].as_str() {
            Some(text) => text.to_owned(),
            None => format!(
                "[embedded resource: {}]",
                resource["uri"].as_str().unwrap_or("unknown")
            ),
        },
        ContentPart::Opaque { .. } => "[unsupported content omitted]".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{InputSource, MessageId, MessageMeta, OpaqueJson, ToolConcurrency, ToolSpec};

    fn config() -> AnthropicConfig {
        AnthropicConfig::new("http://localhost", "claude-test")
    }

    /// The default config with caching off. Tests about the request *mapping* use this so the
    /// shapes stay readable; caching itself is tested by the `cache_control` cases below.
    fn uncached() -> AnthropicConfig {
        config().with_cache(CacheMode::None)
    }

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            system_prompt: None,
            messages,
            tools: Vec::new(),
            settings: Value::Null,
        }
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    #[test]
    fn the_required_fields_and_stream_flag_are_always_sent() {
        let body = build_body(&request(vec![Message::user("hi")]), &uncached()).unwrap();
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type":"text","text":"hi"}]}])
        );
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn host_message_meta_is_not_sent_to_the_provider() {
        let plain = request(vec![Message::user("hello")]);
        let with_meta = request(vec![Message::User {
            content: vec![ContentPart::text("hello")],
            meta: MessageMeta {
                id: Some(MessageId("host-42".into())),
                source: Some(InputSource::Subagent),
                origin: Some(
                    OpaqueJson::parse(
                        r#"{"parent":{"session_id":"s","turn_id":"t","call_id":"c"}}"#,
                    )
                    .unwrap(),
                ),
            },
        }]);
        let plain = serde_json::to_vec(&build_body(&plain, &config()).unwrap()).unwrap();
        let with_meta = serde_json::to_vec(&build_body(&with_meta, &config()).unwrap()).unwrap();
        assert_eq!(with_meta, plain);
        let body = String::from_utf8(with_meta).unwrap();
        assert!(!body.contains("meta") && !body.contains("source") && !body.contains("origin"));
    }

    #[test]
    fn reasoning_is_not_sent_to_the_provider() {
        // The same rule as `meta` (ADR 0005 G4): a history with full reasoning and the same
        // history without it must produce a byte-identical request body. (Anthropic's own
        // extended-thinking round trip is a separate, unimplemented concern; see ADR 0014.)
        let plain = request(vec![Message::user("what?"), Message::assistant("answer")]);
        let with_reasoning = request(vec![
            Message::user("what?"),
            Message::Assistant {
                content: vec![ContentPart::text("answer")],
                reasoning: vec![ContentPart::text("private chain of thought")],
                tool_calls: Vec::new(),
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
        ]);
        let plain_body = serde_json::to_vec(&build_body(&plain, &config()).unwrap()).unwrap();
        let with_body =
            serde_json::to_vec(&build_body(&with_reasoning, &config()).unwrap()).unwrap();
        assert_eq!(with_body, plain_body);
        let body = String::from_utf8(with_body).unwrap();
        assert!(!body.contains("reasoning"), "{body}");
    }

    #[test]
    fn a_tool_conversation_maps_to_anthropic_blocks() {
        let mut req = request(vec![
            Message::user("what is 2+3?"),
            Message::Assistant {
                reasoning: Vec::new(),
                content: Vec::new(),
                tool_calls: vec![call("toolu_1", "calc__add", json!({"a": 2, "b": 3}))],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::Tool {
                result: ToolResult::success("toolu_1", vec![ContentPart::text("5")]),
                meta: MessageMeta::default(),
            },
            Message::Tool {
                result: ToolResult::error("toolu_2", "boom", "it failed"),
                meta: MessageMeta::default(),
            },
            Message::assistant("It is 5."),
        ]);
        req.system_prompt = Some("Be brief.".into());
        req.tools = vec![ToolSpec {
            name: "calc__add".into(),
            description: "Adds.".into(),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Shared,
        }];

        let body = build_body(&req, &uncached()).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "max_tokens": 4096,
                "stream": true,
                "system": "Be brief.",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "what is 2+3?"}]},
                    {"role": "assistant", "content": [{
                        "type": "tool_use", "id": "toolu_1", "name": "calc__add",
                        "input": {"a": 2, "b": 3}
                    }]},
                    // Both tool results are merged into one user message, in order.
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "5"},
                        {"type": "tool_result", "tool_use_id": "toolu_2", "content": "it failed", "is_error": true}
                    ]},
                    {"role": "assistant", "content": [{"type": "text", "text": "It is 5."}]}
                ],
                "tools": [{"name": "calc__add", "description": "Adds.", "input_schema": {"type": "object"}}]
            })
        );
    }

    #[test]
    fn interrupted_assistants_are_sent_as_plain_assistants() {
        let plain = request(vec![
            Message::user("what?"),
            Message::assistant("Let me think..."),
            Message::user("and then?"),
        ]);
        let interrupted = request(vec![
            Message::user("what?"),
            Message::Assistant {
                reasoning: Vec::new(),
                content: vec![ContentPart::text("Let me think...")],
                tool_calls: Vec::new(),
                provider_payload: None,
                interrupted: true,
                meta: MessageMeta::default(),
            },
            Message::user("and then?"),
        ]);
        let plain_body = serde_json::to_string(&build_body(&plain, &config()).unwrap()).unwrap();
        let interrupted_body =
            serde_json::to_string(&build_body(&interrupted, &config()).unwrap()).unwrap();
        assert_eq!(plain_body, interrupted_body);
        assert!(!interrupted_body.contains("interrupted"));
    }

    #[test]
    fn images_are_sent_as_base64_blocks() {
        let body = build_body(
            &request(vec![Message::User {
                content: vec![
                    ContentPart::text("look"),
                    ContentPart::Image {
                        mime_type: "image/png".into(),
                        data: "AAAA".into(),
                        annotations: None,
                        meta: None,
                    },
                ],
                meta: MessageMeta::default(),
            }]),
            &uncached(),
        )
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ])
        );
    }

    #[test]
    fn what_the_api_cannot_take_becomes_a_placeholder_in_text() {
        let body = build_body(
            &request(vec![Message::Tool {
                result: ToolResult::success(
                    "toolu_1",
                    vec![
                        ContentPart::text("done"),
                        ContentPart::Audio {
                            mime_type: "audio/mpeg".into(),
                            data: "BBBB".into(),
                            annotations: None,
                            meta: None,
                        },
                        ContentPart::ResourceLink {
                            uri: "file:///a".into(),
                            name: Some("a".into()),
                            mime_type: None,
                            annotations: None,
                            meta: None,
                        },
                        ContentPart::EmbeddedResource {
                            resource: json!({"uri": "file:///b", "text": "inline"}),
                            annotations: None,
                            meta: None,
                        },
                    ],
                ),
                meta: MessageMeta::default(),
            }]),
            &config(),
        )
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["content"],
            "done\n\n[audio/mpeg audio omitted]\n\n[resource a: file:///a]\n\ninline"
        );
    }

    #[test]
    fn arguments_that_are_not_an_object_become_an_empty_input() {
        let body = build_body(
            &request(vec![Message::Assistant {
                reasoning: Vec::new(),
                content: Vec::new(),
                tool_calls: vec![
                    call("a", "t", Value::String("{\"x\": 1}".into())),
                    call("b", "u", Value::String("not json".into())),
                ],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            }]),
            &config(),
        )
        .unwrap();
        assert_eq!(body["messages"][0]["content"][0]["input"], json!({"x": 1}));
        assert_eq!(body["messages"][0]["content"][1]["input"], json!({}));
    }

    #[test]
    fn settings_override_the_config_and_reserved_keys_are_refused() {
        let mut cfg = config();
        cfg.extra_body.insert("temperature".into(), json!(0.2));
        cfg.extra_body.insert("top_p".into(), json!(0.5));
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!({"temperature": 0.9});

        let body = build_body(&req, &cfg).unwrap();
        assert_eq!(body["temperature"], 0.9);
        assert_eq!(body["top_p"], 0.5);

        for key in [
            "model",
            "messages",
            "max_tokens",
            "stream",
            "system",
            "tools",
        ] {
            let mut req = request(vec![Message::user("hi")]);
            req.settings = json!({ key: 1 });
            assert!(
                matches!(build_body(&req, &cfg), Err(ModelError::Protocol(_))),
                "{key} must be refused"
            );
        }
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!([1, 2]);
        assert!(build_body(&req, &cfg).is_err());
    }

    #[test]
    fn max_tokens_comes_from_the_config() {
        let cfg = config().with_max_tokens(123);
        let body = build_body(&request(vec![Message::user("hi")]), &cfg).unwrap();
        assert_eq!(body["max_tokens"], 123);
    }

    /// Acceptance 5: with thinking off (the default) **and caching off**, the body carries no
    /// `thinking` key and no `cache_control`, so it is byte-identical to the adapter before
    /// either feature existed. (With the default `CacheMode::Auto` the only difference is the
    /// breakpoints; the `cache_control` tests below pin those.)
    #[test]
    fn thinking_off_leaves_the_request_body_unchanged() {
        let body = build_body(&request(vec![Message::user("hi")]), &uncached()).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "max_tokens": 4096,
                "stream": true,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
            })
        );
        assert!(!body.to_string().contains("thinking"), "{body}");
    }

    #[test]
    fn thinking_enabled_sends_the_manual_object() {
        let cfg = config().with_thinking(crate::config::ThinkingConfig::Enabled {
            budget_tokens: 4096,
        });
        let body = build_body(&request(vec![Message::user("hi")]), &cfg).unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 4096})
        );
    }

    #[test]
    fn thinking_adaptive_sends_the_adaptive_object() {
        let cfg = config().with_thinking(crate::config::ThinkingConfig::Adaptive);
        let body = build_body(&request(vec![Message::user("hi")]), &cfg).unwrap();
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    }

    #[test]
    fn configured_thinking_wins_over_extra_body() {
        let mut cfg = config().with_thinking(crate::config::ThinkingConfig::Adaptive);
        cfg.extra_body.insert(
            "thinking".into(),
            json!({"type": "enabled", "budget_tokens": 1}),
        );
        let body = build_body(&request(vec![Message::user("hi")]), &cfg).unwrap();
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    }

    fn assistant_with_payload(payload: &str, calls: Vec<ToolCall>) -> Message {
        Message::Assistant {
            content: vec![ContentPart::text("visible")],
            reasoning: Vec::new(),
            tool_calls: calls,
            provider_payload: Some(OpaqueJson::parse(payload).unwrap()),
            interrupted: false,
            meta: MessageMeta::default(),
        }
    }

    /// Acceptance 6 and 7: the preserved thinking and redacted_thinking blocks go back before
    /// the `tool_use`, in the order they were streamed, with their signatures unchanged.
    #[test]
    fn preserved_thinking_blocks_are_echoed_before_tool_use_in_order() {
        let payload = r#"[{"type":"thinking","thinking":"first","signature":"sig-1"},{"type":"redacted_thinking","data":"opaque"},{"type":"thinking","thinking":"second","signature":"sig-2"}]"#;
        let body = build_body(
            &request(vec![
                Message::user("what?"),
                assistant_with_payload(payload, vec![call("toolu_1", "t", json!({"x": 1}))]),
                Message::Tool {
                    result: ToolResult::success("toolu_1", vec![ContentPart::text("ok")]),
                    meta: MessageMeta::default(),
                },
            ]),
            &config(),
        )
        .unwrap();
        assert_eq!(
            body["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": "first", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "thinking", "thinking": "second", "signature": "sig-2"},
                {"type": "text", "text": "visible"},
                {"type": "tool_use", "id": "toolu_1", "name": "t", "input": {"x": 1}}
            ])
        );
    }

    /// Acceptance 9: a payload that came back from the transcript JSON (as the session file
    /// stores it) still rebuilds the same blocks. The JSONL round trip itself is pinned in
    /// `radian-session`; this pins the adapter side of it.
    #[test]
    fn a_payload_read_back_from_json_still_echoes_thinking() {
        let payload = r#"[{"type":"thinking","thinking":"why","signature":"sig-1"}]"#;
        let message = assistant_with_payload(payload, vec![call("toolu_1", "t", json!({}))]);
        let stored = serde_json::to_string(&message).unwrap();
        let read_back: Message = serde_json::from_str(&stored).unwrap();
        let body = build_body(&request(vec![read_back]), &config()).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({"type": "thinking", "thinking": "why", "signature": "sig-1"})
        );
    }

    #[test]
    fn a_provider_payload_that_is_not_an_array_is_refused() {
        let body = build_body(
            &request(vec![assistant_with_payload(
                r#"{"type":"thinking"}"#,
                vec![],
            )]),
            &config(),
        );
        assert!(matches!(body, Err(ModelError::Protocol(_))), "{body:?}");
    }

    #[test]
    fn an_empty_provider_payload_array_sends_the_plain_assistant() {
        let body = build_body(
            &request(vec![assistant_with_payload("[]", vec![])]),
            &uncached(),
        )
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([{"type": "text", "text": "visible"}])
        );
    }

    // ---------- cross-provider history (ADR 0026 D6) ----------

    /// An assistant turn written by the **other** adapter (an OpenAI-compatible run) must map
    /// cleanly: its `reasoning` is ignored (D6, research §1), its tool ids pass verbatim
    /// (Anthropic only requires `^[a-zA-Z0-9_-]+$`), and its `provider_payload` — which the
    /// OpenAI-compatible adapter never writes — is treated like any other payload.
    #[test]
    fn an_openai_written_history_maps_to_a_messages_request() {
        let tool_call = ToolCall {
            id: "call_1".into(),
            name: "echo".into(),
            arguments: json!({"hello": "mcp"}),
        };
        let history = vec![
            Message::user("run the tool"),
            Message::Assistant {
                content: vec![ContentPart::text("calling it")],
                reasoning: vec![ContentPart::text(
                    "chain of thought from the other provider",
                )],
                tool_calls: vec![tool_call],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::Tool {
                result: ToolResult {
                    call_id: "call_1".into(),
                    content: vec![ContentPart::text("ok")],
                    structured: None,
                    is_error: false,
                    error_code: None,
                },
                meta: MessageMeta::default(),
            },
            Message::user("thanks"),
        ];
        let body = build_body(&request(history), &config()).unwrap();
        let messages = body["messages"].as_array().unwrap();
        // user, assistant, user(tool result), user — the tool result becomes a user turn.
        assert_eq!(messages.len(), 4);
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        // No `thinking` block: the other adapter's reasoning is not echoed, and there was no
        // payload to preserve.
        let blocks = assistant["content"].as_array().unwrap();
        assert!(
            blocks.iter().all(|block| block["type"] != "thinking"),
            "no thinking blocks from a foreign turn: {blocks:?}"
        );
        // The tool id passes verbatim.
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(body["model"], "claude-test");
        // The foreign `reasoning` is nowhere in the body (D6 research §1).
        assert!(!body.to_string().contains("chain of thought"));
    }

    /// A non-array `provider_payload` from a foreign turn is a protocol error, not a silent
    /// drop — dropping a real `thinking` block would make the provider reject the request
    /// (D6: pin today's behavior; a change to "drop and log" is the owner's call).
    #[test]
    fn a_non_array_payload_is_a_protocol_error() {
        let history = vec![
            Message::user("hi"),
            Message::Assistant {
                content: vec![ContentPart::text("answer")],
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
                provider_payload: Some(OpaqueJson::parse(r#"{"type":"thinking"}"#).unwrap()),
                interrupted: false,
                meta: MessageMeta::default(),
            },
        ];
        let error = build_body(&request(history), &config()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("provider_payload must be a JSON array"),
            "{error}"
        );
    }

    /// And the reverse direction: a history this adapter wrote (an assistant turn with a
    /// preserved `thinking` payload) keeps the block when the request is rebuilt.
    #[test]
    fn a_thinking_payload_is_echoed_before_the_text() {
        let payload =
            OpaqueJson::parse(r#"[{"type":"thinking","thinking":"why","signature":"sig"}]"#)
                .unwrap();
        let history = vec![
            Message::user("hi"),
            Message::Assistant {
                content: vec![ContentPart::text("answer")],
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
                provider_payload: Some(payload),
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::user("go on"),
        ];
        let body = build_body(&request(history), &config()).unwrap();
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "why");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "answer");
    }

    /// ADR 0026 D11 (review A1): the adapter's two settings are different requests —
    /// `adaptive` is the provider's own budget, `enabled:<budget>` is a manual one.
    #[test]
    fn adaptive_and_enabled_are_different_bodies() {
        let adaptive = {
            let mut cfg = config();
            cfg.thinking = Some(ThinkingConfig::Adaptive);
            build_body(&request(vec![Message::user("hi")]), &cfg).unwrap()
        };
        let enabled = {
            let mut cfg = config();
            cfg.thinking = Some(ThinkingConfig::Enabled {
                budget_tokens: 8192,
            });
            build_body(&request(vec![Message::user("hi")]), &cfg).unwrap()
        };
        assert_eq!(adaptive["thinking"], json!({"type": "adaptive"}));
        assert_eq!(
            enabled["thinking"],
            json!({"type": "enabled", "budget_tokens": 8192})
        );
        assert_ne!(adaptive["thinking"], enabled["thinking"]);
    }

    // ---------- prompt caching (Anthropic `cache_control`) ----------

    /// The default is [`CacheMode::Auto`], so a host that changes nothing gets caching.
    #[test]
    fn the_default_cache_mode_is_auto() {
        assert_eq!(config().cache, CacheMode::Auto);
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("{name} tool."),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Shared,
        }
    }

    /// A request that exercises all three breakpoint sites.
    fn caching_request() -> ModelRequest {
        let mut req = request(vec![
            Message::user("first"),
            Message::assistant("second"),
            Message::user("third"),
        ]);
        req.system_prompt = Some("Be brief.".into());
        req.tools = vec![tool("add"), tool("mul")];
        req
    }

    /// Auto places exactly one breakpoint on each of the three stated sites: the **last** tool
    /// definition, the end of the system prompt, and the last block of the last message — three
    /// of the four documented slots (the count test below holds the limit).
    #[test]
    fn auto_places_the_three_breakpoints_where_stated() {
        let body = build_body(&caching_request(), &config()).unwrap();

        let tools = body["tools"].as_array().unwrap();
        assert!(
            tools[0].get("cache_control").is_none(),
            "only the last tool definition is a breakpoint: {tools:?}"
        );
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));

        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "Be brief.",
                "cache_control": {"type": "ephemeral"},
            }])
        );

        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["type"], "text");
        assert_eq!(
            last["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(count_cache_breakpoints(&body), 3);
    }

    /// The site is present only when its content is: no tools, no system prompt, no
    /// breakpoint. This also counts the limit (never more than four) on the fullest request.
    #[test]
    fn breakpoints_follow_what_the_request_carries() {
        let cases: [(&str, bool, bool, usize); 4] = [
            ("messages only", false, false, 1),
            ("system only", true, false, 2),
            ("tools only", false, true, 2),
            ("system and tools", true, true, 3),
        ];
        for (name, system, tools, expected) in cases {
            let mut req = request(vec![Message::user("hi")]);
            if system {
                req.system_prompt = Some("Be brief.".into());
            }
            if tools {
                req.tools = vec![tool("add"), tool("mul")];
            }
            let body = build_body(&req, &config()).unwrap();
            assert_eq!(count_cache_breakpoints(&body), expected, "{name}");
            assert!(expected <= 4, "{name}");
            if !system {
                assert!(body.get("system").is_none(), "{name}");
            }
            if !tools {
                assert!(body.get("tools").is_none(), "{name}");
            }
        }
    }

    /// With caching off, the body is exactly the one the adapter sent before prompt caching
    /// existed: system is a plain string, tools carry no `cache_control`, and the messages are
    /// unchanged. Nothing in the body mentions `cache_control`.
    #[test]
    fn none_sends_the_body_the_adapter_sent_before_prompt_caching() {
        let body = build_body(&caching_request(), &uncached()).unwrap();
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "max_tokens": 4096,
                "stream": true,
                "system": "Be brief.",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "first"}]},
                    {"role": "assistant", "content": [{"type": "text", "text": "second"}]},
                    {"role": "user", "content": [{"type": "text", "text": "third"}]}
                ],
                "tools": [
                    {"name": "add", "description": "add tool.", "input_schema": {"type": "object"}},
                    {"name": "mul", "description": "mul tool.", "input_schema": {"type": "object"}}
                ]
            })
        );
        assert!(!body.to_string().contains("cache_control"), "{body}");
        assert_eq!(count_cache_breakpoints(&body), 0);
    }

    /// An empty system prompt keeps the plain-string form: `text` has `minLength 1`, so an empty
    /// block (and therefore a breakpoint on it) may not be built.
    #[test]
    fn auto_leaves_an_empty_system_prompt_as_a_plain_string() {
        let mut req = request(vec![Message::user("hi")]);
        req.system_prompt = Some(String::new());
        let body = build_body(&req, &config()).unwrap();
        assert_eq!(body["system"], "");
        assert_eq!(count_cache_breakpoints(&body), 1, "only the message block");
    }

    /// Thinking and redacted_thinking blocks cannot carry `cache_control`; a last message made
    /// only of them gets no breakpoint rather than an illegal one.
    #[test]
    fn auto_never_marks_a_thinking_block() {
        let payload = r#"[{"type":"thinking","thinking":"why","signature":"sig"},{"type":"redacted_thinking","data":"opaque"}]"#;
        let body = build_body(
            &request(vec![
                Message::user("hi"),
                Message::Assistant {
                    content: Vec::new(),
                    reasoning: Vec::new(),
                    tool_calls: Vec::new(),
                    provider_payload: Some(OpaqueJson::parse(payload).unwrap()),
                    interrupted: false,
                    meta: MessageMeta::default(),
                },
            ]),
            &config(),
        )
        .unwrap();
        assert_eq!(count_cache_breakpoints(&body), 0);
    }

    /// The empty-user fallback is a plain string, not a block array; there is nowhere legal to
    /// put a breakpoint, so Auto adds none.
    #[test]
    fn auto_never_marks_a_string_content_message() {
        let body = build_body(
            &request(vec![Message::User {
                content: Vec::new(),
                meta: MessageMeta::default(),
            }]),
            &config(),
        )
        .unwrap();
        assert_eq!(body["messages"][0]["content"], "");
        assert_eq!(count_cache_breakpoints(&body), 0);
    }

    /// A tool_result block is cacheable; a conversation that ends on one is marked there.
    #[test]
    fn auto_marks_a_trailing_tool_result_block() {
        let body = build_body(
            &request(vec![
                Message::user("what?"),
                Message::Assistant {
                    reasoning: Vec::new(),
                    content: Vec::new(),
                    tool_calls: vec![call("toolu_1", "t", json!({}))],
                    provider_payload: None,
                    interrupted: false,
                    meta: MessageMeta::default(),
                },
                Message::Tool {
                    result: ToolResult::success("toolu_1", vec![ContentPart::text("5")]),
                    meta: MessageMeta::default(),
                },
            ]),
            &config(),
        )
        .unwrap();
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["type"], "tool_result");
        assert_eq!(
            last["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(count_cache_breakpoints(&body), 1);
    }

    /// A tool's `input_schema` and a tool call's arguments may legitimately contain a key named
    /// `cache_control`; those are the caller's data, not breakpoints. The adapter's limit counts
    /// only what it places, so such a request is built normally. (A whole-body scan would count
    /// six keys here and trip the `debug_assert`.)
    #[test]
    fn auto_does_not_mistake_a_user_data_key_for_a_breakpoint() {
        let mut req = request(vec![
            Message::user("hi"),
            Message::Assistant {
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![call(
                    "toolu_1",
                    "t",
                    json!({"cache_control": 1, "nested": {"cache_control": 2}}),
                )],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
        ]);
        req.system_prompt = Some("Be brief.".into());
        req.tools = vec![ToolSpec {
            name: "t".into(),
            description: "t tool.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"cache_control": {"type": "string"}},
            }),
            concurrency: ToolConcurrency::Shared,
        }];

        let body = build_body(&req, &config()).unwrap();

        // The three real breakpoints are where they belong ...
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["content"][0]["type"], "tool_use");
        assert_eq!(
            last["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        // ... and the caller's lookalike keys are left untouched.
        assert_eq!(last["content"][0]["input"]["cache_control"], json!(1));
        assert_eq!(
            last["content"][0]["input"]["nested"]["cache_control"],
            json!(2)
        );
        // Six keys named `cache_control` in total: three breakpoints plus three lookalikes (two
        // in the tool call's arguments, one in the tool's `input_schema`).
        assert_eq!(count_cache_breakpoints(&body), 6);
    }
}
