//! Chat Completions stream chunks -> one complete `ModelResponse`.
//!
//! Only a complete response is returned, as the core requires: a stream that ends without a
//! finish reason is an error, never a partial answer.

use std::collections::HashSet;

use agent_core::{
    AgentEvent, ContentPart, EventSink, ModelError, ModelResponse, StopReason, ToolCall,
};
use serde_json::Value;

#[derive(Debug, Default)]
struct PartialCall {
    index: Option<i64>,
    id: Option<String>,
    name: String,
    arguments: String,
}

pub(crate) struct Assembler<'a> {
    events: &'a dyn EventSink,
    limit: usize,
    used: usize,
    text: String,
    refusal: String,
    calls: Vec<PartialCall>,
    finish_reason: Option<String>,
    done: bool,
}

pub(crate) enum Progress {
    Continue,
    /// The `[DONE]` marker: nothing more will come.
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
            calls: Vec::new(),
            finish_reason: None,
            done: false,
        }
    }

    pub(crate) fn feed(&mut self, data: &str) -> Result<Progress, ModelError> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(Progress::Done);
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::Protocol(format!("unreadable stream chunk: {e}")))?;
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            let message = error["message"].as_str().or(error.as_str());
            return Err(ModelError::Transport(format!(
                "the provider reported an error: {}",
                message.unwrap_or("(no message)")
            )));
        }
        // Chunks with no choice carry only usage data.
        let Some(choice) = chunk["choices"].get(0) else {
            return Ok(Progress::Continue);
        };
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.account(text.len())?;
            self.text.push_str(text);
            self.events
                .emit(AgentEvent::ModelTextDelta { text: text.into() });
        }
        if let Some(text) = delta["refusal"].as_str().filter(|t| !t.is_empty()) {
            self.account(text.len())?;
            self.refusal.push_str(text);
            self.events
                .emit(AgentEvent::ModelTextDelta { text: text.into() });
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.tool_call_fragment(call)?;
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(Progress::Continue)
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

    fn tool_call_fragment(&mut self, fragment: &Value) -> Result<(), ModelError> {
        let index = fragment["index"].as_i64();
        let id = fragment["id"].as_str().filter(|id| !id.is_empty());
        let slot = match index {
            Some(index) => self.calls.iter().position(|c| c.index == Some(index)),
            // Some servers leave `index` out: a new id starts a call, anything else continues
            // the last one.
            None if id.is_some() => None,
            None => self.calls.len().checked_sub(1),
        };
        let slot = match slot {
            Some(slot) => slot,
            None => {
                self.calls.push(PartialCall {
                    index,
                    ..PartialCall::default()
                });
                self.calls.len() - 1
            }
        };
        let name = fragment["function"]["name"].as_str().unwrap_or("");
        let arguments = fragment["function"]["arguments"].as_str().unwrap_or("");
        self.account(name.len() + arguments.len())?;
        let call = &mut self.calls[slot];
        if call.id.is_none() {
            call.id = id.map(str::to_owned);
        }
        call.name.push_str(name);
        call.arguments.push_str(arguments);
        Ok(())
    }

    /// Builds the response. `taken` holds the tool-call ids already in the conversation:
    /// some servers reuse ids such as `call_0` in every response, but the core needs them
    /// unique across the whole transcript, so a repeat is renamed.
    pub(crate) fn finish(self, mut taken: HashSet<String>) -> Result<ModelResponse, ModelError> {
        let reason = match (self.finish_reason, self.done) {
            (Some(reason), _) => reason,
            // `[DONE]` with no reason: infer it rather than discard a finished response.
            (None, true) if !self.calls.is_empty() => "tool_calls".into(),
            (None, true) => "stop".into(),
            (None, false) => {
                return Err(ModelError::Transport(
                    "the stream ended before the response was complete".into(),
                ));
            }
        };

        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !(c.name.is_empty() && c.arguments.is_empty() && c.id.is_none()))
            .map(|(n, call)| ToolCall {
                id: unique_id(call.id.unwrap_or_else(|| format!("call_{n}")), &mut taken),
                name: call.name,
                arguments: parse_arguments(call.arguments),
            })
            .collect();

        let refused = !self.refusal.is_empty() && tool_calls.is_empty();
        let stop_reason = match reason.as_str() {
            "stop" if refused => StopReason::Refusal,
            "stop" => StopReason::EndTurn,
            "tool_calls" | "function_call" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            "content_filter" => StopReason::ContentFiltered,
            other => StopReason::Other(other.to_owned()),
        };
        let text = if self.text.is_empty() {
            self.refusal
        } else {
            self.text
        };
        Ok(ModelResponse {
            content: if text.is_empty() {
                Vec::new()
            } else {
                vec![ContentPart::text(text)]
            },
            tool_calls,
            provider_payload: None,
            stop_reason,
        })
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

    fn chunk(delta: Value, finish: Option<&str>) -> String {
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}).to_string()
    }

    fn assemble(chunks: &[String], done: bool) -> Result<ModelResponse, ModelError> {
        assemble_with(chunks, done, HashSet::new(), usize::MAX)
    }

    fn assemble_with(
        chunks: &[String],
        done: bool,
        taken: HashSet<String>,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, limit);
        for data in chunks {
            assembler.feed(data)?;
        }
        if done {
            assembler.feed("[DONE]")?;
        }
        assembler.finish(taken)
    }

    #[test]
    fn text_deltas_become_one_part_and_stream_as_events() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        for data in [
            chunk(json!({"role": "assistant", "content": ""}), None),
            chunk(json!({"content": "Hel"}), None),
            chunk(json!({"content": "lo"}), None),
            chunk(json!({}), Some("stop")),
        ] {
            assembler.feed(&data).unwrap();
        }
        let response = assembler.finish(HashSet::new()).unwrap();

        assert_eq!(response.content, vec![ContentPart::text("Hello")]);
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert!(response.tool_calls.is_empty());
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
    fn tool_call_fragments_are_joined_by_index() {
        let response = assemble(
            &[
                chunk(
                    json!({"tool_calls": [{"index": 0, "id": "a", "type": "function",
                    "function": {"name": "first", "arguments": ""}}]}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 1, "id": "b", "type": "function",
                    "function": {"name": "second", "arguments": "{\"y\":"}}]}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"x\":"}}]}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 1, "function": {"arguments": "2}"}}]}),
                    None,
                ),
                chunk(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ],
            true,
        )
        .unwrap();

        assert_eq!(response.stop_reason, StopReason::ToolUse);
        let calls: Vec<_> = response
            .tool_calls
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone()))
            .collect();
        assert_eq!(
            calls,
            [
                ("a", "first", json!({"x": 1})),
                ("b", "second", json!({"y": 2}))
            ]
        );
    }

    #[test]
    fn servers_that_omit_the_index_still_work() {
        let response = assemble(
            &[
                chunk(json!({"tool_calls": [{"id": "a", "function": {"name": "f", "arguments": "{\"k\""}}]}), None),
                chunk(json!({"tool_calls": [{"function": {"arguments": ":1}"}}]}), None),
                chunk(json!({"tool_calls": [{"id": "b", "function": {"name": "g", "arguments": "{}"}}]}), None),
                chunk(json!({}), Some("tool_calls")),
            ],
            true,
        )
        .unwrap();
        assert_eq!(response.tool_calls.len(), 2);
        assert_eq!(response.tool_calls[0].arguments, json!({"k": 1}));
        assert_eq!(response.tool_calls[1].name, "g");
    }

    #[test]
    fn missing_ids_are_made_up_and_repeated_ids_are_renamed() {
        let taken: HashSet<String> = ["call_0".to_owned()].into();
        let response = assemble_with(
            &[
                chunk(
                    json!({"tool_calls": [
                        {"index": 0, "function": {"name": "a", "arguments": "{}"}},
                        {"index": 1, "id": "dup", "function": {"name": "b", "arguments": "{}"}},
                        {"index": 2, "id": "dup", "function": {"name": "c", "arguments": "{}"}}
                    ]}),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ],
            true,
            taken,
            usize::MAX,
        )
        .unwrap();

        let ids: Vec<_> = response.tool_calls.iter().map(|c| c.id.clone()).collect();
        // `call_0` was already used earlier in the conversation, so the first becomes `call_0-2`.
        assert_eq!(ids, ["call_0-2", "dup", "dup-2"]);
    }

    #[test]
    fn empty_arguments_mean_none_and_broken_ones_are_kept_as_text() {
        let response = assemble(
            &[
                chunk(
                    json!({"tool_calls": [
                        {"index": 0, "id": "a", "function": {"name": "f", "arguments": ""}},
                        {"index": 1, "id": "b", "function": {"name": "g", "arguments": "{\"x\": "}}
                    ]}),
                    None,
                ),
                chunk(json!({}), Some("length")),
            ],
            false,
        )
        .unwrap();
        assert_eq!(response.tool_calls[0].arguments, json!({}));
        assert_eq!(
            response.tool_calls[1].arguments,
            Value::String("{\"x\": ".into())
        );
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn finish_reasons_map_to_stop_reasons() {
        for (reason, expected) in [
            ("stop", StopReason::EndTurn),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
            ("length", StopReason::MaxTokens),
            ("content_filter", StopReason::ContentFiltered),
            ("something_new", StopReason::Other("something_new".into())),
        ] {
            let response =
                assemble(&[chunk(json!({"content": "x"}), Some(reason))], false).unwrap();
            assert_eq!(response.stop_reason, expected, "{reason}");
        }
    }

    #[test]
    fn a_refusal_is_a_refusal_and_its_text_is_kept() {
        let response = assemble(
            &[
                chunk(json!({"refusal": "I can't "}), None),
                chunk(json!({"refusal": "help with that."}), Some("stop")),
            ],
            true,
        )
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::Refusal);
        assert_eq!(
            response.content,
            vec![ContentPart::text("I can't help with that.")]
        );
    }

    #[test]
    fn a_stream_with_no_finish_reason_is_an_error_not_a_partial_answer() {
        let error = assemble(&[chunk(json!({"content": "half an ans"}), None)], false)
            .err()
            .unwrap();
        assert!(matches!(error, ModelError::Transport(_)), "{error}");
    }

    #[test]
    fn done_without_a_finish_reason_is_inferred() {
        let text = assemble(&[chunk(json!({"content": "hi"}), None)], true).unwrap();
        assert_eq!(text.stop_reason, StopReason::EndTurn);
        let calls = assemble(
            &[chunk(json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": "{}"}}]}), None)],
            true,
        )
        .unwrap();
        assert_eq!(calls.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn an_error_chunk_ends_the_response() {
        let error = assemble(
            &[json!({"error": {"message": "overloaded"}}).to_string()],
            true,
        )
        .err()
        .unwrap();
        assert!(
            matches!(&error, ModelError::Transport(m) if m.contains("overloaded")),
            "{error}"
        );
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        let error = assemble(&["not json".into()], true).err().unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");
    }

    #[test]
    fn usage_only_chunks_are_ignored() {
        let response = assemble(
            &[
                chunk(json!({"content": "ok"}), Some("stop")),
                json!({"choices": [], "usage": {"total_tokens": 3}}).to_string(),
            ],
            true,
        )
        .unwrap();
        assert_eq!(response.content, vec![ContentPart::text("ok")]);
    }

    #[test]
    fn a_response_over_the_limit_is_cut_off() {
        let chunks: Vec<String> = (0..10)
            .map(|_| chunk(json!({"content": "0123456789"}), None))
            .collect();
        let error = assemble_with(&chunks, true, HashSet::new(), 50)
            .err()
            .unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");

        let calls = chunk(
            json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "f", "arguments": "x".repeat(100)}}]}),
            None,
        );
        assert!(assemble_with(&[calls], true, HashSet::new(), 50).is_err());
    }
}
