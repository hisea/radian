//! Responses stream events -> one complete `ModelResponse`.
//!
//! Only a complete response is returned, as the core requires: a stream that ends without a
//! terminal event is an error, never a partial answer. The event names and item shapes are
//! recorded in `crates/agent-provider-openai-responses/WIRE.md`.

use std::collections::HashSet;

use radian_core::{
    AgentEvent, ContentPart, EventSink, ModelError, ModelResponse, ModelUsage, OpaqueJson,
    StopReason, ToolCall,
};
use serde_json::{Map, Value};

#[derive(Debug, Default)]
struct PartialCall {
    index: Option<i64>,
    item_id: Option<String>,
    id: Option<String>,
    name: String,
    /// Arguments accumulated from `response.function_call_arguments.delta`.
    arguments: String,
    /// `response.function_call_arguments.done` / `output_item.done` arguments: authoritative
    /// when present, because a late delta must not corrupt the finished call.
    done_arguments: Option<String>,
}

pub(crate) struct Assembler<'a> {
    events: &'a dyn EventSink,
    limit: usize,
    used: usize,
    text: String,
    refusal: String,
    /// The reasoning **summary** text streamed for the UI. It is the visible projection of the
    /// reasoning channel; the replayable item lives in `reasoning_items` (ADR 0014 R4/R10).
    reasoning: String,
    calls: Vec<PartialCall>,
    /// Completed `reasoning` output items, kept verbatim for `provider_payload`.
    reasoning_items: Vec<Value>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    end_turn: Option<bool>,
    /// The last non-null `response.usage` seen; the terminal event carries the final one.
    usage: Option<Value>,
    done: bool,
}

pub(crate) enum Progress {
    Continue,
    /// A terminal event arrived; nothing more will come.
    Done,
}

impl<'a> Assembler<'a> {
    pub(crate) fn new(events: &'a dyn EventSink, limit: usize) -> Self {
        Self {
            events,
            limit,
            used: 0,
            text: String::new(),
            refusal: String::new(),
            reasoning: String::new(),
            calls: Vec::new(),
            reasoning_items: Vec::new(),
            status: None,
            incomplete_reason: None,
            end_turn: None,
            usage: None,
            done: false,
        }
    }

    pub(crate) fn feed(&mut self, data: &str) -> Result<Progress, ModelError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::Protocol(format!("unreadable stream event: {e}")))?;
        let kind = event["type"].as_str().unwrap_or("");
        match kind {
            "response.created" | "response.in_progress" | "response.queued" => {
                self.record_usage(&event);
            }
            "response.output_item.added" => self.item_added(&event)?,
            "response.output_item.done" => self.item_done(&event)?,
            "response.output_text.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.account(delta.len())?;
                    self.text.push_str(delta);
                    self.events
                        .emit(AgentEvent::ModelTextDelta { text: delta.into() });
                }
            }
            "response.refusal.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.account(delta.len())?;
                    self.refusal.push_str(delta);
                    self.events
                        .emit(AgentEvent::ModelTextDelta { text: delta.into() });
                }
            }
            // The reasoning summary is the streamed reasoning; `response.reasoning_text.delta`
            // is the raw form some servers send instead. Both become a reasoning delta (never a
            // text delta), matching the core's separate reasoning channel (ADR 0014 R1/R9).
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.account(delta.len())?;
                    self.reasoning.push_str(delta);
                    self.events
                        .emit(AgentEvent::ModelReasoningDelta { text: delta.into() });
                }
            }
            "response.function_call_arguments.delta" => {
                let delta = event["delta"].as_str().unwrap_or("");
                if !delta.is_empty() {
                    self.account(delta.len())?;
                    if let Some(index) = self.call_index(&event, None) {
                        self.calls[index].arguments.push_str(delta);
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(arguments) = event["arguments"].as_str()
                    && let Some(index) = self.call_index(&event, None)
                {
                    self.account(arguments.len())?;
                    self.calls[index].done_arguments = Some(arguments.to_owned());
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                self.terminal(kind, &event);
                return Ok(Progress::Done);
            }
            "response.failed" => return Err(self.stream_error(&event)),
            "error" => return Err(self.error_event(&event)),
            // `response.output_text.done`, `content_part.*`, reasoning summary part markers,
            // and any future event kind carry nothing this adapter needs; ignore them.
            _ => {}
        }
        Ok(Progress::Continue)
    }

    fn record_usage(&mut self, event: &Value) {
        if let Some(usage) = event
            .get("response")
            .and_then(|response| response.get("usage"))
            .filter(|usage| usage.is_object())
        {
            self.usage = Some(usage.clone());
        }
    }

    fn item_added(&mut self, event: &Value) -> Result<(), ModelError> {
        let item = &event["item"];
        if item["type"].as_str().unwrap_or("") == "function_call" {
            let item_id = item["id"].as_str().map(str::to_owned);
            let id = item["call_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .or_else(|| item_id.clone());
            let name = item["name"].as_str().unwrap_or("").to_owned();
            let arguments = item["arguments"].as_str().unwrap_or("").to_owned();
            self.account(name.len() + arguments.len())?;
            self.calls.push(PartialCall {
                index: event["output_index"].as_i64(),
                item_id,
                id,
                name,
                arguments,
                done_arguments: None,
            });
        }
        Ok(())
    }

    fn item_done(&mut self, event: &Value) -> Result<(), ModelError> {
        let item = &event["item"];
        match item["type"].as_str().unwrap_or("") {
            "reasoning" => {
                // Keep the item exactly as received: the provider mints the encrypted content
                // and expects it back unchanged (ADR 0014 R10).
                self.reasoning_items.push(item.clone());
                let summary = join_text(item.get("summary"), "text");
                let content = join_text(item.get("content"), "text");
                let authoritative = if !summary.is_empty() {
                    summary
                } else {
                    content
                };
                if !authoritative.is_empty() {
                    self.reasoning = authoritative;
                }
            }
            "message" => {
                let (text, refusal) = message_text(item);
                if !text.is_empty() {
                    self.text = text;
                }
                if !refusal.is_empty() {
                    self.refusal = refusal;
                }
            }
            "function_call" => {
                let item_id = item["id"].as_str();
                let call_id = item["call_id"].as_str().filter(|id| !id.is_empty());
                let name = item["name"].as_str().unwrap_or("");
                let arguments = item["arguments"].as_str().unwrap_or("");
                self.account(name.len() + arguments.len())?;
                match self.call_index(event, item_id) {
                    Some(index) => {
                        let call = &mut self.calls[index];
                        if call.id.is_none() {
                            call.id = call_id.map(str::to_owned);
                        }
                        if !name.is_empty() {
                            call.name = name.to_owned();
                        }
                        call.done_arguments = Some(arguments.to_owned());
                    }
                    // `output_item.added` never arrived (a lossy proxy): synthesize the call so
                    // the consumer still receives what it was told completed.
                    None => self.calls.push(PartialCall {
                        index: event["output_index"].as_i64(),
                        item_id: item_id.map(str::to_owned),
                        id: call_id.map(str::to_owned),
                        name: name.to_owned(),
                        arguments: String::new(),
                        done_arguments: Some(arguments.to_owned()),
                    }),
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The index of the open call a delta/done event belongs to, by `item_id` (best), then by
    /// `output_index`. `item_id` is passed explicitly for `output_item.done` because the event
    /// may not carry a top-level `item_id`.
    fn call_index(&self, event: &Value, item_id: Option<&str>) -> Option<usize> {
        let item_id = item_id.or_else(|| event["item_id"].as_str());
        if let Some(item_id) = item_id
            && let Some(index) = self
                .calls
                .iter()
                .position(|call| call.item_id.as_deref() == Some(item_id))
        {
            return Some(index);
        }
        if let Some(output_index) = event["output_index"].as_i64()
            && let Some(index) = self
                .calls
                .iter()
                .position(|call| call.index == Some(output_index))
        {
            return Some(index);
        }
        None
    }

    fn terminal(&mut self, kind: &str, event: &Value) {
        self.record_usage(event);
        let response = &event["response"];
        let status = response["status"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| match kind {
                "response.incomplete" => "incomplete".into(),
                _ => "completed".into(),
            });
        self.status = Some(status);
        if let Some(end_turn) = response["end_turn"].as_bool() {
            self.end_turn = Some(end_turn);
        }
        if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
            self.incomplete_reason = Some(reason.to_owned());
        }
        self.done = true;
    }

    fn stream_error(&self, event: &Value) -> ModelError {
        let error = &event["response"]["error"];
        self.classify_error(error)
    }

    fn error_event(&self, event: &Value) -> ModelError {
        let error = if event.get("error").is_some() {
            &event["error"]
        } else {
            event
        };
        self.classify_error(error)
    }

    fn classify_error(&self, error: &Value) -> ModelError {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .unwrap_or("(no message)");
        let text = format!("the provider reported an error: {message}");
        if crate::overflow::classify_stream_overflow(error) {
            return ModelError::ContextOverflow(text);
        }
        ModelError::Transport(text)
    }

    fn account(&mut self, bytes: usize) -> Result<(), ModelError> {
        self.used = self.used.saturating_add(bytes);
        if self.used > self.limit {
            return Err(ModelError::Protocol(format!(
                "the response is longer than the limit of {} bytes",
                self.limit
            )));
        }
        Ok(())
    }

    /// Builds the complete response. `taken` holds the tool-call ids already in the
    /// conversation: a repeated or missing provider id is renamed or made up so the history
    /// stays unique across the whole transcript.
    pub(crate) fn finish(self, mut taken: HashSet<String>) -> Result<ModelResponse, ModelError> {
        if !self.done {
            return Err(ModelError::Transport(
                "the stream ended before the response was complete".into(),
            ));
        }
        let status = self.status.as_deref().unwrap_or("completed");
        let mut stop_reason = match status {
            "completed" | "in_progress" | "queued" => StopReason::EndTurn,
            "incomplete" => match self.incomplete_reason.as_deref() {
                // The Responses API names a content filter in `incomplete_details.reason`.
                Some("content_filter") => StopReason::ContentFiltered,
                _ => StopReason::MaxTokens,
            },
            // A brand-new status must surface, not silently become a normal end (the contract
            // checks this for `Reason::Unknown`).
            other => StopReason::Other(other.to_owned()),
        };

        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, call)| {
                !(call.name.is_empty()
                    && call.id.is_none()
                    && call.arguments.is_empty()
                    && call.done_arguments.as_deref().unwrap_or("").is_empty())
            })
            .map(|(n, call)| {
                let raw = call.done_arguments.unwrap_or(call.arguments);
                ToolCall {
                    id: unique_id(call.id.unwrap_or_else(|| format!("call_{n}")), &mut taken),
                    name: call.name,
                    arguments: parse_arguments(raw),
                }
            })
            .collect();

        if !tool_calls.is_empty() && matches!(stop_reason, StopReason::EndTurn) {
            stop_reason = StopReason::ToolUse;
        }
        if self.end_turn == Some(false) && matches!(stop_reason, StopReason::EndTurn) {
            stop_reason = StopReason::PauseTurn;
        }
        if !self.refusal.is_empty()
            && tool_calls.is_empty()
            && matches!(stop_reason, StopReason::EndTurn)
        {
            stop_reason = StopReason::Refusal;
        }

        let text = if self.text.is_empty() {
            self.refusal
        } else {
            self.text
        };
        Ok(ModelResponse {
            reasoning: if self.reasoning.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(self.reasoning)]
            },
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(text)]
            },
            tool_calls,
            provider_payload: provider_payload(self.reasoning_items),
            stop_reason,
            usage: self.usage.as_ref().map(usage_from_json),
            // The model layer fills this from the HTTP response headers.
            limits: None,
        })
    }
}

/// The completed `reasoning` items, as one opaque JSON array for `provider_payload`. `None`
/// when the response had no reasoning item, so a non-reasoning response stays unchanged.
fn provider_payload(items: Vec<Value>) -> Option<OpaqueJson> {
    if items.is_empty() {
        return None;
    }
    Some(
        OpaqueJson::parse(Value::Array(items).to_string())
            .expect("a JSON array built from JSON values is valid JSON"),
    )
}

fn join_text(parts: Option<&Value>, key: &str) -> String {
    let Some(Value::Array(parts)) = parts else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|part| part[key].as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn message_text(item: &Value) -> (String, String) {
    let Some(Value::Array(parts)) = item.get("content") else {
        return (String::new(), String::new());
    };
    let mut text = String::new();
    let mut refusal = String::new();
    for part in parts {
        match part["type"].as_str() {
            Some("output_text") => text.push_str(part["text"].as_str().unwrap_or("")),
            Some("refusal") => refusal.push_str(part["refusal"].as_str().unwrap_or("")),
            _ => {}
        }
    }
    (text, refusal)
}

/// Maps a Responses usage object to the neutral [`ModelUsage`]. The wire's
/// `input_tokens_details`/`output_tokens_details` sub-objects are recorded under the neutral
/// ADR 0007 J3 keys `prompt_tokens_details`/`completion_tokens_details` (the OpenAI-shaped
/// convention `details` uses); `cache_read`/`cache_write` still come from the input details.
/// `input_includes_cache` is `Some(true)`: Responses' `input_tokens` **contains**
/// `input_tokens_details.cached_tokens` (ADR 0007). `total` is the provider's own,
/// never computed.
fn usage_from_json(usage: &Value) -> ModelUsage {
    let number = |key: &str| usage.get(key).and_then(Value::as_u64);
    let input_details = usage.get("input_tokens_details").filter(|d| d.is_object());
    let output_details = usage.get("output_tokens_details").filter(|d| d.is_object());
    let details = {
        let mut merged = Map::new();
        if let Some(sub) = input_details {
            merged.insert("prompt_tokens_details".into(), sub.clone());
        }
        if let Some(sub) = output_details {
            merged.insert("completion_tokens_details".into(), sub.clone());
        }
        if merged.is_empty() {
            None
        } else {
            Some(
                OpaqueJson::parse(Value::Object(merged).to_string())
                    .expect("a usage sub-object is valid JSON"),
            )
        }
    };
    let details_number = |key: &str| input_details.and_then(|d| d.get(key).and_then(Value::as_u64));
    ModelUsage {
        input: number("input_tokens"),
        output: number("output_tokens"),
        total: number("total_tokens"),
        cache_read: details_number("cached_tokens"),
        cache_write: details_number("cache_write_tokens"),
        input_includes_cache: Some(true),
        details,
    }
}

/// An empty string means "no arguments". Text that is not JSON is kept as a string value, which
/// the core's argument check rejects with a message the model can read and correct.
fn parse_arguments(raw: String) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Default::default());
    }
    serde_json::from_str(&raw).unwrap_or(Value::String(raw))
}

fn unique_id(id: String, taken: &mut HashSet<String>) -> String {
    if taken.insert(id.clone()) {
        return id;
    }
    (2..)
        .map(|n| format!("{id}-{n}"))
        .find(|candidate| taken.insert(candidate.clone()))
        .expect("an unused suffix exists")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<AgentEvent>>);

    impl EventSink for Recorder {
        fn emit(&self, event: AgentEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn assemble(events: &[Value]) -> Result<ModelResponse, ModelError> {
        assemble_with(events, HashSet::new(), usize::MAX)
    }

    fn assemble_with(
        events: &[Value],
        taken: HashSet<String>,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, limit);
        for event in events {
            assembler.feed(&event.to_string())?;
        }
        assembler.finish(taken)
    }

    fn completed(status: &str) -> Value {
        json!({"type": "response.completed",
               "response": {"id": "resp_1", "status": status, "usage": null}})
    }

    fn message_item(text: &str) -> Value {
        json!({"type": "message", "id": "msg_1", "role": "assistant",
               "content": [{"type": "output_text", "text": text}]})
    }

    #[test]
    fn text_deltas_become_one_part_and_stream_as_events() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        for event in [
            json!({"type": "response.output_text.delta", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "delta": "lo"}),
            completed("completed"),
        ] {
            assembler.feed(&event.to_string()).unwrap();
        }
        let response = assembler.finish(HashSet::new()).unwrap();
        assert_eq!(response.content, vec![ContentPart::text("Hello")]);
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            [
                AgentEvent::ModelTextDelta { text: "Hel".into() },
                AgentEvent::ModelTextDelta { text: "lo".into() },
            ]
        );
    }

    #[test]
    fn a_completed_message_item_is_authoritative_over_the_deltas() {
        let response = assemble(&[
            json!({"type": "response.output_text.delta", "delta": "half"}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": message_item("the full answer")}),
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(response.content, vec![ContentPart::text("the full answer")]);
    }

    #[test]
    fn a_refusal_is_a_refusal_and_its_text_is_kept() {
        let response = assemble(&[
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}}),
            json!({"type": "response.refusal.delta", "delta": "I can't help."}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "message", "id": "msg_1", "role": "assistant",
                            "content": [{"type": "refusal", "refusal": "I can't help."}]}}),
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(response.content, vec![ContentPart::text("I can't help.")]);
    }

    #[test]
    fn reasoning_summary_streams_as_reasoning_not_text() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        for event in [
            json!({"type": "response.reasoning_summary_text.delta", "delta": "think"}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "ing"}),
            json!({"type": "response.output_text.delta", "delta": "answer"}),
            completed("completed"),
        ] {
            assembler.feed(&event.to_string()).unwrap();
        }
        let response = assembler.finish(HashSet::new()).unwrap();
        assert_eq!(response.reasoning, vec![ContentPart::text("thinking")]);
        assert_eq!(response.content, vec![ContentPart::text("answer")]);
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            [
                AgentEvent::ModelReasoningDelta {
                    text: "think".into()
                },
                AgentEvent::ModelReasoningDelta { text: "ing".into() },
                AgentEvent::ModelTextDelta {
                    text: "answer".into()
                },
            ]
        );
    }

    #[test]
    fn a_reasoning_item_is_kept_verbatim_in_the_provider_payload() {
        let item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "the summary"}],
            "encrypted_content": "OPAQUEBLOB"
        });
        let response = assemble(&[
            json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "the summary"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": message_item("ok")}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": message_item("ok")}),
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(response.reasoning, vec![ContentPart::text("the summary")]);
        let payload = response
            .provider_payload
            .expect("the reasoning item is kept");
        let payload: Value = serde_json::from_str(payload.get()).unwrap();
        assert_eq!(payload, json!([item]));
    }

    #[test]
    fn a_response_without_reasoning_has_no_provider_payload() {
        let response = assemble(&[completed("completed")]).unwrap();
        assert!(response.provider_payload.is_none());
        assert!(response.reasoning.is_empty());
    }

    #[test]
    fn tool_call_arguments_split_across_deltas_rejoin() {
        let response = assemble(&[
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "function_call", "id": "fc_1", "call_id": "c1",
                            "name": "calc", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1",
                   "output_index": 0, "delta": "{\"a\":"}),
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1",
                   "output_index": 0, "delta": "2}"}),
            // No authoritative `done`: the call must be assembled from the deltas alone, so
            // dropping one is a visible error.
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "c1");
        assert_eq!(response.tool_calls[0].name, "calc");
        assert_eq!(response.tool_calls[0].arguments, json!({"a": 2}));
    }

    #[test]
    fn several_tool_calls_keep_their_order() {
        let response = assemble(&[
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "a", "name": "one", "arguments": "{\"x\":1}"}}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"type": "function_call", "call_id": "b", "name": "two", "arguments": "{\"y\":2}"}}),
            completed("completed"),
        ])
        .unwrap();
        let calls: Vec<_> = response
            .tool_calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone()))
            .collect();
        assert_eq!(
            calls,
            [("a", "one", json!({"x": 1})), ("b", "two", json!({"y": 2}))]
        );
    }

    #[test]
    fn missing_ids_are_made_up_and_repeated_ids_are_renamed() {
        let taken: HashSet<String> = ["call_0".to_owned()].into();
        let response = assemble_with(
            &[
                json!({"type": "response.output_item.done", "output_index": 0,
                       "item": {"type": "function_call", "name": "a", "arguments": "{}"}}),
                json!({"type": "response.output_item.done", "output_index": 1,
                       "item": {"type": "function_call", "call_id": "dup", "name": "b", "arguments": "{}"}}),
                json!({"type": "response.output_item.done", "output_index": 2,
                       "item": {"type": "function_call", "call_id": "dup", "name": "c", "arguments": "{}"}}),
                completed("completed"),
            ],
            taken,
            usize::MAX,
        )
        .unwrap();
        let ids: Vec<_> = response.tool_calls.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, ["call_0-2", "dup", "dup-2"]);
    }

    #[test]
    fn broken_arguments_are_kept_as_text() {
        let response = assemble(&[
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"x\": "}}),
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(
            response.tool_calls[0].arguments,
            Value::String("{\"x\": ".into())
        );
    }

    #[test]
    fn stop_reasons_map_from_status_and_markers() {
        for (status, expected) in [
            ("completed", StopReason::EndTurn),
            ("in_progress", StopReason::EndTurn),
            ("queued", StopReason::EndTurn),
            ("something_new", StopReason::Other("something_new".into())),
        ] {
            let response = assemble(&[completed(status)]).unwrap();
            assert_eq!(response.stop_reason, expected, "{status}");
        }
        // incomplete -> MaxTokens; a content filter is its own reason.
        let max = assemble(&[
            json!({"type": "response.incomplete",
                   "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}}),
        ])
        .unwrap();
        assert_eq!(max.stop_reason, StopReason::MaxTokens);
        let filtered = assemble(&[
            json!({"type": "response.incomplete",
                   "response": {"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}}),
        ])
        .unwrap();
        assert_eq!(filtered.stop_reason, StopReason::ContentFiltered);
        // end_turn: false pauses the turn (codex).
        let paused = assemble(&[json!({"type": "response.completed",
                   "response": {"status": "completed", "usage": null, "end_turn": false}})])
        .unwrap();
        assert_eq!(paused.stop_reason, StopReason::PauseTurn);
    }

    #[test]
    fn a_stream_without_a_terminal_event_is_an_error() {
        let error = assemble(&[json!({"type": "response.output_text.delta", "delta": "half"})])
            .err()
            .unwrap();
        assert!(matches!(error, ModelError::Transport(_)), "{error}");
    }

    #[test]
    fn a_failed_response_is_a_transport_error() {
        let error = assemble(&[json!({"type": "response.failed",
            "response": {"status": "failed", "error": {"code": "server_error", "message": "boom"}}})])
            .err()
            .unwrap();
        assert!(
            matches!(&error, ModelError::Transport(m) if m.contains("boom")),
            "{error}"
        );
    }

    #[test]
    fn an_error_event_with_a_context_overflow_code_is_an_overflow() {
        let error = assemble(&[json!({"type": "error",
            "error": {"code": "context_length_exceeded", "message": "too big"}})])
        .err()
        .unwrap();
        assert!(matches!(error, ModelError::ContextOverflow(_)), "{error}");
    }

    #[test]
    fn unknown_events_are_ignored() {
        let response = assemble(&[
            json!({"type": "response.future_event", "x": 1}),
            json!({"type": "response.output_text.delta", "delta": "ok"}),
            completed("completed"),
        ])
        .unwrap();
        assert_eq!(response.content, vec![ContentPart::text("ok")]);
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        let error = assembler.feed("not json").err().unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");
    }

    #[test]
    fn usage_maps_responses_fields_and_keeps_the_details() {
        let response = assemble(&[
            json!({"type": "response.completed", "response": {"status": "completed", "usage": {
                "input_tokens": 11,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens": 4,
                "output_tokens_details": {"reasoning_tokens": 30},
                "total_tokens": 15
            }}}),
        ])
        .unwrap();
        let usage = response.usage.expect("usage was reported");
        assert_eq!(usage.input, Some(11));
        assert_eq!(usage.output, Some(4));
        assert_eq!(usage.total, Some(15));
        assert_eq!(usage.cache_read, Some(3));
        assert_eq!(usage.cache_write, None);
        assert_eq!(usage.input_includes_cache, Some(true));
        let details: Value = serde_json::from_str(usage.details.unwrap().get()).unwrap();
        assert_eq!(
            details,
            json!({
                "prompt_tokens_details": {"cached_tokens": 3},
                "completion_tokens_details": {"reasoning_tokens": 30}
            })
        );
    }

    #[test]
    fn the_last_non_null_usage_wins() {
        let response = assemble(&[
            json!({"type": "response.created",
                   "response": {"status": "in_progress", "usage": {"input_tokens": 1}}}),
            json!({"type": "response.completed",
                   "response": {"status": "completed", "usage": {"input_tokens": 9, "output_tokens": 2}}}),
        ])
        .unwrap();
        let usage = response.usage.unwrap();
        assert_eq!(usage.input, Some(9));
        assert_eq!(usage.output, Some(2));
    }

    #[test]
    fn no_usage_means_none_and_total_is_never_invented() {
        let response = assemble(&[completed("completed")]).unwrap();
        assert_eq!(response.usage, None);
        let response = assemble(&[
            json!({"type": "response.completed", "response": {"status": "completed",
                   "usage": {"input_tokens": 7, "output_tokens": 2}}}),
        ])
        .unwrap();
        assert_eq!(response.usage.unwrap().total, None);
    }

    #[test]
    fn a_response_over_the_limit_is_cut_off() {
        let events: Vec<Value> = (0..10)
            .map(|_| json!({"type": "response.output_text.delta", "delta": "0123456789"}))
            .collect();
        let error = assemble_with(&events, HashSet::new(), 50).err().unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");
    }
}
