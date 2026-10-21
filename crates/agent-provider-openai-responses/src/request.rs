//! `ModelRequest` -> Responses request body. Pure, so it is tested without a server.
//!
//! Field names and shapes are recorded with their source in
//! `crates/agent-provider-openai-responses/WIRE.md`; the wire is the standard Responses
//! `input` item list (also used by the Codex endpoint).

use radian_core::{ContentPart, Message, ModelError, ModelRequest, ToolCall, ToolResult};
use serde_json::{Map, Value, json};

use crate::config::OpenAiResponsesConfig;

/// Keys the adapter owns. Settings may not override them. `reasoning_effort` is **not** here:
/// it is translated to `reasoning.effort` and may come from `extra_body`/settings.
const RESERVED: [&str; 7] = [
    "model",
    "input",
    "instructions",
    "tools",
    "stream",
    "store",
    "include",
];

pub(crate) fn build_body(
    request: &ModelRequest,
    config: &OpenAiResponsesConfig,
) -> Result<Value, ModelError> {
    let mut body = Map::new();
    let settings = settings_object(&request.settings)?;
    // ADR 0018: a request-level `reasoning_effort` (the summary call's
    // own, lower reasoning setting) wins over the adapter config. It is translated to
    // `reasoning.effort`, never written as a top-level key (that is the Chat Completions name).
    let mut request_effort: Option<String> = None;
    for source in [&config.extra_body, &settings] {
        for (key, value) in source {
            if RESERVED.contains(&key.as_str()) {
                return Err(ModelError::Protocol(format!(
                    "the request setting `{key}` is set by the adapter and cannot be overridden"
                )));
            }
            if key == "reasoning_effort" {
                request_effort = value.as_str().map(str::to_owned);
                continue;
            }
            body.insert(key.clone(), value.clone());
        }
    }
    body.insert("model".into(), json!(config.model));
    // ADR 0026: the subscription endpoint is stateless for us.
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    // Ask for the encrypted reasoning so a later turn can replay it verbatim (ADR 0014 R10).
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(system) = &request.system_prompt {
        body.insert("instructions".into(), json!(system));
    }
    // `reasoning` may already be present from `extra_body`/settings (a full object); only fill
    // it from the effort/summary dials when nothing set it.
    if !body.contains_key("reasoning") {
        let effort = request_effort.or_else(|| config.reasoning_effort.clone());
        let summary = config.reasoning_summary.clone();
        if effort.is_some() || summary.is_some() {
            let mut reasoning = Map::new();
            if let Some(effort) = effort {
                reasoning.insert("effort".into(), json!(effort));
            }
            if let Some(summary) = summary {
                reasoning.insert("summary".into(), json!(summary));
            }
            body.insert("reasoning".into(), Value::Object(reasoning));
        }
    }

    body.insert("input".into(), Value::Array(input_items(&request.messages)));

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
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

/// The Responses `input` array. An assistant turn is a `reasoning` item (replayed verbatim),
/// an assistant `message`, and one `function_call` per tool call; a tool result is a
/// `function_call_output`.
fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        match message {
            Message::User { content, .. } => items.push(user_message(content)),
            Message::Assistant {
                content,
                tool_calls,
                provider_payload,
                ..
            } => {
                // ADR 0014 R4/R10: the visible `reasoning` text is provider-invisible and is
                // never sent; only the provider's own replayable items (with
                // `encrypted_content`) are echoed back, verbatim.
                replay_reasoning(provider_payload.as_ref(), &mut items);
                let text = plain_text(content);
                if !text.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for call in tool_calls {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": arguments_string(call),
                    }));
                }
            }
            Message::Tool { result, .. } => {
                items.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id,
                    "output": tool_output(result),
                }));
            }
        }
    }
    items
}

/// Replays the reasoning items this adapter stored (ADR 0014 R10). The payload is
/// the JSON array of `reasoning` output items from the producing response, kept byte-for-byte.
/// A payload from another provider (e.g. Anthropic `thinking` blocks) or malformed JSON is
/// ignored rather than misinterpreted: cross-provider histories must stay valid (ADR 0026 D6).
fn replay_reasoning(payload: Option<&radian_core::OpaqueJson>, items: &mut Vec<Value>) {
    let Some(payload) = payload else { return };
    let Ok(Value::Array(blocks)) = serde_json::from_str::<Value>(payload.get()) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("reasoning") {
            items.push(block);
        }
    }
}

fn user_message(content: &[ContentPart]) -> Value {
    let parts: Vec<Value> = content.iter().map(input_part).collect();
    json!({"type": "message", "role": "user", "content": parts})
}

fn input_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text, .. } => json!({"type": "input_text", "text": text}),
        ContentPart::Image {
            mime_type, data, ..
        } => json!({
            "type": "input_image",
            "image_url": format!("data:{mime_type};base64,{data}"),
        }),
        // The Responses input vocabulary has no audio part; a short text placeholder keeps the
        // turn valid and tells the model something was there.
        ContentPart::Audio { mime_type, .. } => {
            json!({"type": "input_text", "text": format!("[{mime_type} audio omitted]")})
        }
        ContentPart::ResourceLink { uri, name, .. } => {
            let text = match name {
                Some(name) => format!("[resource {name}: {uri}]"),
                None => format!("[resource: {uri}]"),
            };
            json!({"type": "input_text", "text": text})
        }
        ContentPart::EmbeddedResource { resource, .. } => {
            let text = match resource["text"].as_str() {
                Some(text) => text.to_owned(),
                None => format!(
                    "[embedded resource: {}]",
                    resource["uri"].as_str().unwrap_or("unknown")
                ),
            };
            json!({"type": "input_text", "text": text})
        }
        ContentPart::Opaque { .. } => {
            json!({"type": "input_text", "text": "[unsupported content omitted]"})
        }
    }
}

/// The API carries function arguments as a JSON string. Arguments the model sent that were not
/// valid JSON are stored as a string value by the core, so they go back exactly as received.
fn arguments_string(call: &ToolCall) -> String {
    match &call.arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

fn tool_output(result: &ToolResult) -> String {
    let mut text = plain_text(&result.content);
    if result.is_error {
        text = format!("Error: {text}");
    }
    text
}

/// Text of every part, joined by blank lines, as the Chat Completions adapter does.
fn plain_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_core::{InputSource, MessageId, MessageMeta, OpaqueJson, ToolConcurrency, ToolSpec};

    fn config() -> OpenAiResponsesConfig {
        OpenAiResponsesConfig::new("http://localhost/v1", "test-model")
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
    fn the_body_carries_the_fixed_responses_keys() {
        let mut req = request(vec![Message::user("hi")]);
        req.system_prompt = Some("Be brief.".into());
        let body = build_body(&req, &config()).unwrap();
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["store"], false, "store must be false");
        assert_eq!(body["stream"], true);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["instructions"], "Be brief.");
        assert_eq!(
            body["input"],
            json!([{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}]
            }])
        );
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn a_tool_conversation_maps_to_input_items() {
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
        req.tools = vec![ToolSpec {
            name: "calc__add".into(),
            description: "Adds.".into(),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Shared,
        }];
        let body = build_body(&req, &config()).unwrap();
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "what is 2+3?"}]},
                {"type": "function_call", "call_id": "c1", "name": "calc__add",
                 "arguments": "{\"a\":2,\"b\":3}"},
                {"type": "function_call_output", "call_id": "c1", "output": "5"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "It is 5."}]}
            ])
        );
        assert_eq!(
            body["tools"],
            json!([{"type": "function", "name": "calc__add", "description": "Adds.",
                    "parameters": {"type": "object"}}])
        );
    }

    #[test]
    fn visible_reasoning_text_is_never_sent() {
        // ADR 0014 R4: the non-replayable reasoning channel is provider-invisible.
        let with_reasoning = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: vec![ContentPart::text("private chain of thought")],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&with_reasoning, &config()).unwrap();
        let text = body.to_string();
        assert!(!text.contains("private chain of thought"), "{text}");
        assert!(!text.contains("\"reasoning\""), "{text}");
    }

    #[test]
    fn the_providers_reasoning_item_is_replayed_verbatim() {
        // ADR 0014 R10: the reasoning output item (id, summary, encrypted_content)
        // is echoed back unchanged, before the assistant message.
        let item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "thought"}],
            "encrypted_content": "OPAQUEBLOB"
        });
        let req = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: vec![ContentPart::text("thought")],
            tool_calls: Vec::new(),
            provider_payload: Some(OpaqueJson::parse(json!([item.clone()]).to_string()).unwrap()),
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&req, &config()).unwrap();
        assert_eq!(body["input"][0], item);
        assert_eq!(body["input"][1]["type"], "message");
    }

    #[test]
    fn a_foreign_provider_payload_is_ignored() {
        // Cross-provider (ADR 0026 D6): Anthropic thinking blocks are not Responses reasoning
        // items, so they are dropped rather than sent as `reasoning`.
        let req = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            provider_payload: Some(
                OpaqueJson::parse(r#"[{"type":"thinking","thinking":"x","signature":"sig"}]"#)
                    .unwrap(),
            ),
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&req, &config()).unwrap();
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert!(!body.to_string().contains("thinking"));
    }

    #[test]
    fn malformed_provider_payload_is_ignored() {
        let req = request(vec![Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            provider_payload: Some(OpaqueJson::parse(r#"{"not":"an array"}"#).unwrap()),
            interrupted: false,
            meta: MessageMeta::default(),
        }]);
        let body = build_body(&req, &config()).unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
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
    fn images_become_input_images_and_audio_a_placeholder() {
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
        )
        .unwrap();
        assert_eq!(
            body["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                {"type": "input_text", "text": "[audio/mpeg audio omitted]"}
            ])
        );
    }

    #[test]
    fn reasoning_effort_is_sent_only_when_set() {
        let mut cfg = config();
        cfg.reasoning_effort = Some("high".into());
        cfg.reasoning_summary = Some("auto".into());
        let body = build_body(&request(vec![Message::user("hi")]), &cfg).unwrap();
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );

        let plain = build_body(&request(vec![Message::user("hi")]), &config()).unwrap();
        assert!(plain.get("reasoning").is_none(), "{plain}");

        // A request setting wins over the config (the summary call's lower level).
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!({"reasoning_effort": "low"});
        let overridden = build_body(&req, &cfg).unwrap();
        assert_eq!(overridden["reasoning"]["effort"], "low");
        assert!(overridden.get("reasoning_effort").is_none());
    }

    #[test]
    fn settings_override_the_config_and_reserved_keys_are_refused() {
        let mut cfg = config();
        cfg.extra_body.insert("temperature".into(), json!(0.2));
        cfg.extra_body
            .insert("max_output_tokens".into(), json!(100));
        let mut req = request(vec![Message::user("hi")]);
        req.settings = json!({"temperature": 0.9});
        let body = build_body(&req, &cfg).unwrap();
        assert_eq!(body["temperature"], 0.9);
        assert_eq!(body["max_output_tokens"], 100);

        for key in RESERVED {
            let mut req = request(vec![Message::user("hi")]);
            req.settings = json!({ key: 1 });
            assert!(
                matches!(build_body(&req, &cfg), Err(ModelError::Protocol(_))),
                "{key} must be refused"
            );
        }
    }

    #[test]
    fn a_tool_error_is_marked_for_the_model() {
        let body = build_body(
            &request(vec![Message::Tool {
                result: ToolResult::error("c1", "boom", "it failed"),
                meta: MessageMeta::default(),
            }]),
            &config(),
        )
        .unwrap();
        assert_eq!(body["input"][0]["output"], "Error: it failed");
    }
}
