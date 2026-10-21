//! `ModelRequest` -> Chat Completions request body. Pure, so it is tested without a server.

use agent_core::{ContentPart, Message, ModelError, ModelRequest, ToolCall, ToolResult};
use serde_json::{Map, Value, json};

use crate::config::OpenAiCompatibleConfig;

/// Keys the adapter owns. Settings may not override them. `reasoning_effort` is **not** here:
/// since ADR 0018 it is a request-level setting that wins over `config.reasoning_effort`.
const RESERVED: [&str; 5] = ["model", "messages", "tools", "stream", "stream_options"];

pub(crate) fn build_body(
    request: &ModelRequest,
    config: &OpenAiCompatibleConfig,
    include_usage: bool,
) -> Result<Value, ModelError> {
    let mut body = Map::new();
    let settings = settings_object(&request.settings)?;
    // ADR 0018: a request-level `reasoning_effort` (the summary call's
    // own, lower reasoning setting) wins over the adapter config. The config still wins over
    // `extra_body` (its own dial), so the guard is on the **request settings**.
    let request_effort = settings.contains_key("reasoning_effort");
    for source in [&config.extra_body, &settings] {
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
    body.insert("stream".into(), json!(true));
    // ADR 0026 D11: the thinking level's wire value, when the route's map gives one. Absent
    // otherwise, so a level-less model keeps the pre-D11 request byte for byte. A request-level
    // value (ADR 0018) wins over it.
    if !request_effort && let Some(effort) = &config.reasoning_effort {
        body.insert("reasoning_effort".into(), json!(effort));
    }
    // Only ask for streamed usage when we handle it; some compatible servers reject the key.
    if include_usage {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }

    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if let Some(system) = &request.system_prompt {
        messages.push(json!({"role": "system", "content": system}));
    }
    let replay = config.reasoning_replay.as_deref();
    messages.extend(request.messages.iter().map(|item| message(item, replay)));
    body.insert("messages".into(), Value::Array(messages));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    Ok(Value::Object(body))
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

fn message(message: &Message, replay: Option<&str>) -> Value {
    match message {
        Message::User { content, .. } => user(content),
        Message::Assistant {
            content,
            reasoning,
            tool_calls,
            ..
        } => assistant(content, reasoning, tool_calls, replay),
        Message::Tool { result, .. } => tool_result(result),
    }
}

fn user(content: &[ContentPart]) -> Value {
    let has_media = content
        .iter()
        .any(|p| matches!(p, ContentPart::Image { .. } | ContentPart::Audio { .. }));
    if !has_media {
        return json!({"role": "user", "content": plain_text(content)});
    }
    let parts: Vec<Value> = content
        .iter()
        .map(|part| match part {
            ContentPart::Image {
                mime_type, data, ..
            } => json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime_type};base64,{data}")}
            }),
            ContentPart::Audio {
                mime_type, data, ..
            } => json!({
                "type": "input_audio",
                "input_audio": {"data": data, "format": audio_format(mime_type)}
            }),
            other => json!({"type": "text", "text": text_of(other)}),
        })
        .collect();
    json!({"role": "user", "content": parts})
}

fn assistant(
    content: &[ContentPart],
    reasoning: &[ContentPart],
    tool_calls: &[ToolCall],
    replay: Option<&str>,
) -> Value {
    let text = plain_text(content);
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    // The API wants `content` null, not an empty string, when only tool calls were made.
    let content = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    message.insert("content".into(), content);
    // ADR 0014 Q1/Q3: when the model requires it, the reasoning is replayed under
    // the field the model named. Every assistant message carries the key, an empty string when
    // the message has no reasoning, so a history mixing reasoning and non-reasoning replies is
    // still accepted (DeepSeek's "all subsequent requests" rule).
    if let Some(field) = replay {
        message.insert(field.into(), json!(reasoning_text(reasoning)));
    }
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": arguments_string(call)},
                })
            })
            .collect();
        message.insert("tool_calls".into(), Value::Array(calls));
    }
    Value::Object(message)
}

/// The API carries arguments as a JSON string. A string value is what `response` stores for
/// arguments the model sent that were not valid JSON, so it goes back exactly as received.
fn arguments_string(call: &ToolCall) -> String {
    match &call.arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

fn tool_result(result: &ToolResult) -> Value {
    let mut text = plain_text(&result.content);
    if result.is_error {
        text = format!("Error: {text}");
    }
    json!({"role": "tool", "tool_call_id": result.call_id, "content": text})
}

/// The reasoning the model wants back (ADR 0014 Q1): the text parts only, joined
/// by newlines, matching pi. Non-text parts are dropped rather than placeheld — a signature or
/// structured reason belongs to `provider_payload`, not this channel (ADR 0014 R3/R10).
fn reasoning_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text of every part, joined by blank lines. Parts the API cannot take as text become short
/// placeholders, so the model knows something was there. This is lossy by design.
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

fn audio_format(mime_type: &str) -> &str {
    match mime_type {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        other => other.strip_prefix("audio/").unwrap_or(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{InputSource, MessageId, MessageMeta, OpaqueJson, ToolConcurrency, ToolSpec};

    fn config() -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig::new("http://localhost/v1", "test-model")
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

        let plain = serde_json::to_vec(&build_body(&plain, &config(), false).unwrap()).unwrap();
        let with_meta =
            serde_json::to_vec(&build_body(&with_meta, &config(), false).unwrap()).unwrap();
        assert_eq!(with_meta, plain);
        let body = String::from_utf8(with_meta).unwrap();
        assert!(!body.contains("meta") && !body.contains("source") && !body.contains("origin"));
    }

    #[test]
    fn reasoning_is_not_sent_to_the_provider() {
        // The same rule as `meta` (ADR 0005 G4): a history with full reasoning and the same
        // history without it must produce a byte-identical request body.
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
        let plain_body =
            serde_json::to_vec(&build_body(&plain, &config(), false).unwrap()).unwrap();
        let with_body =
            serde_json::to_vec(&build_body(&with_reasoning, &config(), false).unwrap()).unwrap();
        assert_eq!(with_body, plain_body);
        let body = String::from_utf8(with_body).unwrap();
        assert!(!body.contains("reasoning"), "{body}");
    }

    #[test]
    fn reasoning_is_replayed_only_when_the_model_asks_for_it() {
        // ADR 0014 Q1/Q3: with a replay field, an assistant message's reasoning is
        // written under that key, and a message with none carries an empty string.
        let mut cfg = config();
        cfg.reasoning_replay = Some("reasoning_content".into());
        let req = request(vec![
            Message::user("what?"),
            Message::Assistant {
                content: vec![ContentPart::text("answer")],
                reasoning: vec![
                    ContentPart::text("first thought"),
                    ContentPart::text("second thought"),
                ],
                tool_calls: Vec::new(),
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::assistant("plain reply"),
        ]);
        let body = build_body(&req, &cfg, false).unwrap();
        assert_eq!(
            body["messages"][1]["reasoning_content"],
            "first thought\nsecond thought"
        );
        assert_eq!(body["messages"][2]["reasoning_content"], "");
        // The reasoning never leaks into the visible content.
        assert_eq!(body["messages"][1]["content"], "answer");
    }

    #[test]
    fn the_replay_field_name_follows_the_config() {
        let mut cfg = config();
        cfg.reasoning_replay = Some("reasoning".into());
        let req = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: vec![ContentPart::text("thought")],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&req, &cfg, false).unwrap();
        assert_eq!(body["messages"][0]["reasoning"], "thought");
        assert!(body["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn a_non_text_reasoning_part_is_not_replayed() {
        // R3/R10: a signature or other non-text block is not this channel's payload, so the
        // replay keeps only the text parts.
        let mut cfg = config();
        cfg.reasoning_replay = Some("reasoning_content".into());
        let req = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: vec![
                ContentPart::text("visible thought"),
                ContentPart::Image {
                    mime_type: "image/png".into(),
                    data: "AAAA".into(),
                    annotations: None,
                    meta: None,
                },
            ],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&req, &cfg, false).unwrap();
        assert_eq!(body["messages"][0]["reasoning_content"], "visible thought");
    }

    #[test]
    fn a_tool_conversation_maps_to_the_expected_messages() {
        let mut req = request(vec![
            Message::user("what is 2+3?"),
            Message::Assistant {
                reasoning: Vec::new(),
                content: Vec::new(),
                tool_calls: vec![call("c1", "calc__add", json!({"a": 2, "b": 3}))],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::Tool {
                result: ToolResult::success("c1", vec![ContentPart::text("5")]),
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

        let body = build_body(&req, &config(), false).unwrap();

        assert_eq!(
            body,
            json!({
                "model": "test-model",
                "stream": true,
                "messages": [
                    {"role": "system", "content": "Be brief."},
                    {"role": "user", "content": "what is 2+3?"},
                    {"role": "assistant", "content": null, "tool_calls": [{
                        "id": "c1", "type": "function",
                        "function": {"name": "calc__add", "arguments": "{\"a\":2,\"b\":3}"}
                    }]},
                    {"role": "tool", "tool_call_id": "c1", "content": "5"},
                    {"role": "assistant", "content": "It is 5."}
                ],
                "tools": [{"type": "function", "function": {
                    "name": "calc__add", "description": "Adds.",
                    "parameters": {"type": "object"}
                }}]
            })
        );
    }

    #[test]
    fn interrupted_assistants_are_sent_as_plain_assistants() {
        // Same content, only the flag differs: the wire bodies must be byte-identical.
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
        let plain_body =
            serde_json::to_string(&build_body(&plain, &config(), false).unwrap()).unwrap();
        let interrupted_body =
            serde_json::to_string(&build_body(&interrupted, &config(), false).unwrap()).unwrap();
        assert_eq!(plain_body, interrupted_body);
        assert!(!interrupted_body.contains("interrupted"));
    }

    #[test]
    fn no_tools_means_no_tools_key() {
        let body = build_body(&request(vec![Message::user("hi")]), &config(), false).unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn stream_options_carries_include_usage_only_when_enabled() {
        let req = request(vec![Message::user("hi")]);
        let on = build_body(&req, &config(), true).unwrap();
        assert_eq!(on["stream_options"], json!({ "include_usage": true }));
        let off = build_body(&req, &config(), false).unwrap();
        assert!(off.get("stream_options").is_none());
    }

    #[test]
    fn stream_options_is_reserved_like_stream() {
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!({ "stream_options": { "include_usage": true } });
        assert!(matches!(
            build_body(&req, &config(), true),
            Err(ModelError::Protocol(_))
        ));
    }

    #[test]
    fn arguments_that_were_not_json_go_back_as_received() {
        let body = build_body(
            &request(vec![Message::Assistant {
                reasoning: Vec::new(),
                content: vec![ContentPart::text("calling")],
                tool_calls: vec![call("c1", "t", Value::String("{\"a\": ".into()))],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            }]),
            &config(),
            false,
        )
        .unwrap();
        let message = &body["messages"][0];
        assert_eq!(message["content"], "calling");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"a\": "
        );
    }

    #[test]
    fn a_tool_error_is_marked_for_the_model() {
        let body = build_body(
            &request(vec![Message::Tool {
                result: ToolResult::error("c1", "boom", "it failed"),
                meta: MessageMeta::default(),
            }]),
            &config(),
            false,
        )
        .unwrap();
        assert_eq!(body["messages"][0]["content"], "Error: it failed");
    }

    #[test]
    fn images_and_audio_are_sent_where_the_api_takes_them() {
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
                    ContentPart::Audio {
                        mime_type: "audio/mpeg".into(),
                        data: "BBBB".into(),
                        annotations: None,
                        meta: None,
                    },
                ],
                meta: MessageMeta::default(),
            }]),
            &config(),
            false,
        )
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "input_audio", "input_audio": {"data": "BBBB", "format": "mp3"}}
            ])
        );
    }

    #[test]
    fn what_the_api_cannot_take_becomes_a_placeholder_in_text() {
        let body = build_body(
            &request(vec![Message::Tool {
                result: ToolResult::success(
                    "c1",
                    vec![
                        ContentPart::text("done"),
                        ContentPart::Image {
                            mime_type: "image/png".into(),
                            data: "AAAA".into(),
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
            false,
        )
        .unwrap();
        assert_eq!(
            body["messages"][0]["content"],
            "done\n\n[image/png image omitted]\n\n[resource a: file:///a]\n\ninline"
        );
    }

    #[test]
    fn settings_override_the_config_and_reserved_keys_are_refused() {
        let mut cfg = config();
        cfg.extra_body.insert("temperature".into(), json!(0.2));
        cfg.extra_body.insert("max_tokens".into(), json!(100));
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!({"temperature": 0.9});

        let body = build_body(&req, &cfg, false).unwrap();
        assert_eq!(body["temperature"], 0.9);
        assert_eq!(body["max_tokens"], 100);

        for key in ["model", "messages", "tools", "stream"] {
            let mut req = request(vec![Message::user("hi")]);
            req.settings = json!({ key: 1 });
            assert!(
                matches!(build_body(&req, &cfg, false), Err(ModelError::Protocol(_))),
                "{key} must be refused"
            );
        }
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!([1, 2]);
        assert!(build_body(&req, &cfg, false).is_err());
    }
    // ---------- cross-provider history (ADR 0026 D6) ----------

    /// A history written by the **other** adapter (an Anthropic run) must map cleanly: its
    /// `reasoning` is not read (D6, research §1), its preserved `thinking` payload is not
    /// read either (this adapter never writes one and must not invent one), and the tool ids
    /// pass verbatim.
    #[test]
    fn an_anthropic_written_history_maps_to_a_chat_request() {
        let tool_call = ToolCall {
            id: "toolu_01".into(),
            name: "echo".into(),
            arguments: json!({"hello": "mcp"}),
        };
        let history = vec![
            Message::user("run the tool"),
            Message::Assistant {
                content: vec![ContentPart::text("calling it")],
                reasoning: vec![ContentPart::text("chain of thought from anthropic")],
                tool_calls: vec![tool_call],
                provider_payload: Some(
                    OpaqueJson::parse(r#"[{"type":"thinking","thinking":"why"}]"#).unwrap(),
                ),
                interrupted: false,
                meta: MessageMeta::default(),
            },
            Message::Tool {
                result: ToolResult {
                    call_id: "toolu_01".into(),
                    content: vec![ContentPart::text("ok")],
                    structured: None,
                    is_error: false,
                    error_code: None,
                },
                meta: MessageMeta::default(),
            },
            Message::user("thanks"),
        ];
        let body = build_body(&request(history), &config(), false).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4, "{messages:?}");
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["tool_calls"][0]["id"], "toolu_01");
        // Neither the foreign reasoning nor its payload reaches the wire.
        let body_text = body.to_string();
        assert!(!body_text.contains("chain of thought"));
        assert!(!body_text.contains("thinking"));
        assert_eq!(body["model"], "test-model");
    }

    /// ADR 0026 D11: `reasoning_effort` rides along only when the route's map gave a wire
    /// value; without one the request is unchanged (byte-for-byte with pre-D11).
    /// ADR 0018: a request-level `reasoning_effort` wins over the config.
    #[test]
    fn reasoning_effort_is_sent_only_when_set() {
        let mut cfg = config();
        cfg.reasoning_effort = Some("high".into());
        let body = build_body(&request(vec![Message::user("hi")]), &cfg, false).unwrap();
        assert_eq!(body["reasoning_effort"], "high");

        let plain = build_body(&request(vec![Message::user("hi")]), &config(), false).unwrap();
        assert!(plain.get("reasoning_effort").is_none(), "{plain}");

        // The request setting wins over the config (the summary call's lower level).
        let mut override_request = request(vec![Message::user("hi")]);
        override_request.settings = json!({"reasoning_effort": "low"});
        let overridden = build_body(&override_request, &cfg, false).unwrap();
        assert_eq!(overridden["reasoning_effort"], "low");

        // The config is not invented when nothing set it.
        let mut no_effort = request(vec![Message::user("hi")]);
        no_effort.settings = json!({"temperature": 0.2});
        let none = build_body(&no_effort, &config(), false).unwrap();
        assert!(none.get("reasoning_effort").is_none(), "{none}");
    }
}
