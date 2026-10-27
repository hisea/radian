//! `session/load`: the journal's transcript as `session/update`s (ADR 0049).
//!
//! The transcript is the one the journal holds — the same messages a TUI session resumes and
//! `radian exec --resume` sends to the model, not a separate history. The replay is therefore a
//! pure function of it: a user message becomes `user_message_chunk`, an assistant message its
//! `agent_thought_chunk` and `agent_message_chunk`, each tool call a `tool_call` followed by the
//! `tool_call_update` of its committed result. Nothing is written; the loaded session's journal
//! keeps only the new turn.

use std::collections::HashMap;
use std::path::Path;

use radian_core::{ContentPart, Message};
use serde_json::Value;

use super::protocol;

/// The `session/update`s for one journal transcript, in transcript order. An assistant message
/// carries its reasoning and its text separately on disk, so the reasoning chunks come first
/// (the order a live client saw them in); a tool call's result follows the assistant message
/// that requested it.
pub(crate) fn updates(messages: &[Message], workspace: &Path) -> Vec<Value> {
    let mut updates = Vec::new();
    // `call_id -> (name, arguments)` from the committed assistant message, so a result's
    // `tool_call_update` carries the title the `tool_call` gave it.
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    for message in messages {
        match message {
            Message::User { content, .. } => {
                for part in content {
                    if let Some(text) = part_text(part) {
                        updates.push(protocol::text_chunk("user_message_chunk", &text));
                    }
                }
            }
            Message::Assistant {
                content,
                reasoning,
                tool_calls,
                ..
            } => {
                for part in reasoning {
                    if let Some(text) = part_text(part) {
                        updates.push(protocol::text_chunk("agent_thought_chunk", &text));
                    }
                }
                for part in content {
                    if let Some(text) = part_text(part) {
                        updates.push(protocol::text_chunk("agent_message_chunk", &text));
                    }
                }
                for call in tool_calls {
                    calls.insert(call.id.clone(), (call.name.clone(), call.arguments.clone()));
                    updates.push(protocol::tool_call(
                        &call.id,
                        &call.name,
                        &call.arguments,
                        workspace,
                    ));
                }
            }
            Message::Tool { result, .. } => {
                let (name, arguments) = calls
                    .get(&result.call_id)
                    .cloned()
                    .unwrap_or_else(|| (String::new(), Value::Null));
                let title = protocol::tool_title(&name, &arguments);
                updates.push(protocol::tool_call_update(
                    &result.call_id,
                    &title,
                    result.is_error,
                    Some(protocol::tool_output(result)),
                ));
            }
        }
    }
    updates
}

/// One content part as a text block. A resource link becomes the same `[name](uri)` line a
/// `session/prompt` would have produced; an image, an audio block or an embedded resource has no
/// text form here and is not replayed.
fn part_text(part: &ContentPart) -> Option<String> {
    match part {
        ContentPart::Text { text, .. } => Some(text.clone()),
        ContentPart::ResourceLink { uri, name, .. } => {
            let name = name.as_deref().unwrap_or(uri);
            Some(format!("[{name}]({uri})"))
        }
        _ => None,
    }
}
