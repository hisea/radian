//! Anthropic Messages stream events -> one complete `ModelResponse`.
//!
//! Only a complete response is returned, as the core requires: a stream that ends without a
//! `message_stop` (and a `message_delta` carrying `stop_reason`) is an error, never a partial
//! answer. Event shapes and the `stop_reason` set come from the official documentation, with
//! URLs and access dates in `docs/providers/anthropic-wire-notes.md`.

use std::collections::HashSet;

use agent_core::{
    AgentEvent, ContentPart, EventSink, ModelError, ModelResponse, ModelUsage, OpaqueJson,
    StopReason, ToolCall,
};
use serde_json::Value;

#[derive(Debug, Default)]
struct PartialCall {
    id: Option<String>,
    name: String,
    /// `input` from `content_block_start` (usually `{}`), used only when no delta arrives.
    start_input: Value,
    partial_json: String,
    saw_delta: bool,
}

/// One `thinking` or `redacted_thinking` block, accumulated so it can be passed back to the
/// provider unchanged (ADR 0014 R10). `start` is the `content_block_start` object verbatim, so
/// fields this adapter does not know still round-trip; `thinking`/`signature` are filled from
/// the deltas. A `redacted_thinking` block carries its opaque `data` in `start` and gets no
/// deltas.
#[derive(Debug)]
struct ThinkingAccum {
    start: Value,
    thinking: String,
    signature: String,
}

enum Block {
    Text(String),
    Tool(PartialCall),
    /// A `thinking`/`redacted_thinking` block, preserved for `provider_payload`. It is never
    /// turned into text or reasoning: it is provider-visible protocol data (ADR 0014 R10).
    Thinking(ThinkingAccum),
    /// A block kind the adapter does not implement (server tools, ...). Its deltas are
    /// ignored, never turned into text or tool calls.
    Ignored,
}

#[derive(Debug, Default)]
struct UsageAccum {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    details: Option<Value>,
}

impl UsageAccum {
    /// Anthropic's `message_delta` usage is cumulative, so the last value seen for a field
    /// wins; `message_start` supplies input/cache and `message_delta` the output count.
    fn merge(&mut self, usage: &Value) {
        let number = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(value) = number("input_tokens") {
            self.input = Some(value);
        }
        if let Some(value) = number("output_tokens") {
            self.output = Some(value);
        }
        if let Some(value) = number("cache_read_input_tokens") {
            self.cache_read = Some(value);
        }
        if let Some(value) = number("cache_creation_input_tokens") {
            self.cache_write = Some(value);
        }
        // Anthropic has exactly one usage sub-object (`output_tokens_details`), so unlike the
        // OpenAI-compatible adapter (which keys several `*_details` sub-objects by their wire
        // names) this one keeps the sub-object verbatim. The wire notes record both sides.
        if let Some(details) = usage.get("output_tokens_details").filter(|d| d.is_object()) {
            self.details = Some(details.clone());
        }
    }

    fn to_model_usage(&self) -> ModelUsage {
        ModelUsage {
            input: self.input,
            output: self.output,
            // Anthropic's usage object has no total field (wire notes §3); never compute it.
            total: None,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
            // Anthropic's `input_tokens` is the **uncached** part, a sibling of
            // `cache_read_input_tokens`/`cache_creation_input_tokens` (ADR 0007).
            input_includes_cache: Some(false),
            details: self.details.as_ref().map(|d| {
                OpaqueJson::parse(d.to_string()).expect("a usage sub-object is valid JSON")
            }),
        }
    }
}

pub(crate) struct Assembler<'a> {
    events: &'a dyn EventSink,
    limit: usize,
    used: usize,
    blocks: Vec<Block>,
    stop_reason: Option<String>,
    usage: Option<UsageAccum>,
    done: bool,
}

pub(crate) enum Progress {
    Continue,
    /// `message_stop`: nothing more will come.
    Done,
}

impl<'a> Assembler<'a> {
    pub(crate) fn new(events: &'a dyn EventSink, limit: usize) -> Self {
        Self {
            events,
            limit,
            used: 0,
            blocks: Vec::new(),
            stop_reason: None,
            usage: None,
            done: false,
        }
    }

    pub(crate) fn feed(&mut self, data: &str) -> Result<Progress, ModelError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::Protocol(format!("unreadable stream event: {e}")))?;
        match event["type"].as_str().unwrap_or("") {
            "ping" => {}
            "error" => {
                let error = &event["error"];
                let message = error["message"].as_str().or_else(|| error.as_str());
                let text = format!(
                    "the provider reported an error: {}",
                    message.unwrap_or("(no message)")
                );
                // ADR 0018: a stream error carries no HTTP status, so only the error `type`
                // and message can identify an overflow here.
                if crate::overflow::classify_stream_overflow(error) {
                    return Err(ModelError::ContextOverflow(text));
                }
                return Err(ModelError::Transport(text));
            }
            "message_start" => {
                let usage = &event["message"]["usage"];
                if usage.is_object() {
                    self.merge_usage(usage);
                }
            }
            "content_block_start" => {
                let index = index_of(&event);
                let block = &event["content_block"];
                let started = match block["type"].as_str().unwrap_or("") {
                    "text" => Block::Text(block["text"].as_str().unwrap_or("").to_owned()),
                    "tool_use" => {
                        let name = block["name"].as_str().unwrap_or("").to_owned();
                        self.account(name.len())?;
                        Block::Tool(PartialCall {
                            id: block["id"]
                                .as_str()
                                .filter(|id| !id.is_empty())
                                .map(str::to_owned),
                            name,
                            start_input: block.get("input").cloned().unwrap_or(Value::Null),
                            partial_json: String::new(),
                            saw_delta: false,
                        })
                    }
                    // thinking / redacted_thinking: preserved verbatim for the provider_payload
                    // so a tool turn can echo it back (ADR 0014 R10). This is **not** the
                    // reasoning channel: thinking text must never become visible content or a
                    // UI event.
                    "thinking" | "redacted_thinking" => Block::Thinking(ThinkingAccum {
                        start: block.clone(),
                        thinking: block["thinking"].as_str().unwrap_or("").to_owned(),
                        signature: block["signature"].as_str().unwrap_or("").to_owned(),
                    }),
                    // server_tool_use, web_search_tool_result, ...: not implemented.
                    _ => Block::Ignored,
                };
                self.set_block(index, started);
            }
            "content_block_delta" => {
                let index = index_of(&event);
                let delta = &event["delta"];
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        if let Some(text) = delta["text"].as_str().filter(|t| !t.is_empty()) {
                            self.account(text.len())?;
                            match self.blocks.get_mut(index) {
                                Some(Block::Text(existing)) => existing.push_str(text),
                                _ => self.set_block(index, Block::Text(text.to_owned())),
                            }
                            self.events
                                .emit(AgentEvent::ModelTextDelta { text: text.into() });
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta["partial_json"].as_str() {
                            self.account(partial.len())?;
                            if let Some(Block::Tool(call)) = self.blocks.get_mut(index) {
                                call.partial_json.push_str(partial);
                                call.saw_delta = true;
                            }
                        }
                    }
                    // Thinking deltas are kept in the block, never emitted as text or
                    // reasoning (ADR 0014 R10): emitting them would corrupt the core's
                    // interrupted prefix (ADR 0003 F2) and leak hidden reasoning to the UI.
                    "thinking_delta" => {
                        if let Some(text) = delta["thinking"].as_str() {
                            self.account(text.len())?;
                            if let Some(Block::Thinking(acc)) = self.blocks.get_mut(index) {
                                acc.thinking.push_str(text);
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) = delta["signature"].as_str() {
                            self.account(signature.len())?;
                            if let Some(Block::Thinking(acc)) = self.blocks.get_mut(index) {
                                acc.signature.push_str(signature);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"]
                    .as_str()
                    .filter(|r| !r.is_empty())
                {
                    self.stop_reason = Some(reason.to_owned());
                }
                let usage = &event["usage"];
                if usage.is_object() {
                    self.merge_usage(usage);
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(Progress::Done);
            }
            // The versioning policy says new event types may appear; ignore them gracefully.
            _ => {}
        }
        Ok(Progress::Continue)
    }

    fn merge_usage(&mut self, usage: &Value) {
        self.usage
            .get_or_insert_with(UsageAccum::default)
            .merge(usage);
    }

    fn set_block(&mut self, index: usize, block: Block) {
        while self.blocks.len() <= index {
            self.blocks.push(Block::Ignored);
        }
        self.blocks[index] = block;
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

    /// Builds the response. `taken` holds the tool-call ids already in the conversation: a
    /// repeated or missing provider id is renamed or made up so the history stays unique.
    pub(crate) fn finish(self, mut taken: HashSet<String>) -> Result<ModelResponse, ModelError> {
        if !self.done {
            return Err(ModelError::Transport(
                "the stream ended before the response was complete".into(),
            ));
        }
        let Some(reason) = self.stop_reason else {
            return Err(ModelError::Transport(
                "the stream ended without a stop reason".into(),
            ));
        };

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut preserved = Vec::new();
        for (n, block) in self.blocks.into_iter().enumerate() {
            match block {
                Block::Text(text) if !text.is_empty() => content.push(ContentPart::text(text)),
                Block::Text(_) | Block::Ignored => {}
                Block::Thinking(acc) => {
                    let ThinkingAccum {
                        mut start,
                        thinking,
                        signature,
                    } = acc;
                    // A redacted block carries its opaque `data` in `start`; leave it as-is.
                    // A regular block takes the deltas' thinking/signature, so the signature
                    // value is byte-identical to what the provider sent.
                    if start["type"].as_str() == Some("thinking") {
                        start["thinking"] = Value::String(thinking);
                        start["signature"] = Value::String(signature);
                    }
                    preserved.push(start);
                }
                Block::Tool(call) => {
                    let arguments = call_arguments(&call);
                    tool_calls.push(ToolCall {
                        id: unique_id(call.id.unwrap_or_else(|| format!("toolu_{n}")), &mut taken),
                        name: call.name,
                        arguments,
                    });
                }
            }
        }
        Ok(ModelResponse {
            reasoning: Vec::new(),
            content,
            tool_calls,
            provider_payload: provider_payload(preserved),
            stop_reason: map_stop_reason(&reason),
            usage: self.usage.as_ref().map(UsageAccum::to_model_usage),
        })
    }
}

/// The blocks to pass back to the provider, as one opaque JSON array (ADR 0014 R10). `None`
/// when the response had no thinking/redacted_thinking blocks, so a thinking-free response is
/// unchanged.
fn provider_payload(blocks: Vec<Value>) -> Option<OpaqueJson> {
    if blocks.is_empty() {
        return None;
    }
    Some(
        OpaqueJson::parse(Value::Array(blocks).to_string())
            .expect("a JSON array built from JSON values is valid JSON"),
    )
}

fn index_of(event: &Value) -> usize {
    event["index"].as_u64().unwrap_or(0) as usize
}

/// Anthropic `stop_reason` -> neutral `StopReason`. The two lossy neutralizations and their
/// reasons are recorded in `docs/providers/anthropic-wire-notes.md` §5.
fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "end_turn" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "pause_turn" => StopReason::PauseTurn,
        "refusal" => StopReason::Refusal,
        // No core equivalent, but the response finished normally: treat it as a natural end.
        "stop_sequence" => StopReason::EndTurn,
        // The documentation says to treat this as truncated; `MaxTokens` also keeps the core
        // from executing tool calls that may have been cut off.
        "model_context_window_exceeded" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_owned()),
    }
}

fn call_arguments(call: &PartialCall) -> Value {
    if call.saw_delta {
        parse_arguments(call.partial_json.clone())
    } else if call.start_input.is_object() {
        call.start_input.clone()
    } else {
        Value::Object(Default::default())
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

    fn start(index: usize, block: Value) -> String {
        json!({"type": "content_block_start", "index": index, "content_block": block}).to_string()
    }

    fn delta(index: usize, delta: Value) -> String {
        json!({"type": "content_block_delta", "index": index, "delta": delta}).to_string()
    }

    fn stop(index: usize) -> String {
        json!({"type": "content_block_stop", "index": index}).to_string()
    }

    fn message_delta(reason: &str) -> String {
        json!({"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": null}})
            .to_string()
    }

    fn message_stop() -> String {
        json!({"type": "message_stop"}).to_string()
    }

    fn message_start(usage: Option<Value>) -> String {
        let mut message = json!({
            "id": "msg_test", "type": "message", "role": "assistant", "content": [],
            "model": "claude-test", "stop_reason": null, "stop_sequence": null
        });
        if let Some(usage) = usage {
            message["usage"] = usage;
        }
        json!({"type": "message_start", "message": message}).to_string()
    }

    fn assemble(events: &[String]) -> Result<ModelResponse, ModelError> {
        assemble_with(events, HashSet::new(), usize::MAX)
    }

    fn assemble_with(
        events: &[String],
        taken: HashSet<String>,
        limit: usize,
    ) -> Result<ModelResponse, ModelError> {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, limit);
        for data in events {
            assembler.feed(data)?;
        }
        assembler.finish(taken)
    }

    #[test]
    fn text_deltas_become_one_part_and_stream_as_events() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        for data in [
            message_start(Some(json!({"input_tokens": 25, "output_tokens": 1}))),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "Hel"})),
            delta(0, json!({"type": "text_delta", "text": "lo"})),
            stop(0),
            message_delta("end_turn"),
            message_stop(),
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
    fn multiple_text_blocks_stay_separate_parts() {
        let response = assemble(&[
            message_start(None),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "one"})),
            stop(0),
            start(1, json!({"type": "text", "text": ""})),
            delta(1, json!({"type": "text_delta", "text": "two"})),
            stop(1),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        assert_eq!(
            response.content,
            vec![ContentPart::text("one"), ContentPart::text("two")]
        );
    }

    #[test]
    fn a_tool_use_block_with_partial_json_is_assembled() {
        let response = assemble(&[
            message_start(None),
            start(
                1,
                json!({"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}}),
            ),
            delta(
                1,
                json!({"type": "input_json_delta", "partial_json": "{\"location\":"}),
            ),
            delta(
                1,
                json!({"type": "input_json_delta", "partial_json": " \"SF\"}"}),
            ),
            stop(1),
            message_delta("tool_use"),
            message_stop(),
        ])
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_1");
        assert_eq!(response.tool_calls[0].name, "get_weather");
        assert_eq!(response.tool_calls[0].arguments, json!({"location": "SF"}));
    }

    #[test]
    fn a_tool_use_input_from_the_start_block_is_used_without_deltas() {
        let response = assemble(&[
            message_start(None),
            start(
                0,
                json!({"type": "tool_use", "id": "toolu_1", "name": "t", "input": {"x": 1}}),
            ),
            stop(0),
            message_delta("tool_use"),
            message_stop(),
        ])
        .unwrap();
        assert_eq!(response.tool_calls[0].arguments, json!({"x": 1}));
    }

    #[test]
    fn thinking_blocks_are_preserved_and_never_become_content() {
        let sink = Recorder::default();
        let mut assembler = Assembler::new(&sink, usize::MAX);
        for data in [
            message_start(None),
            start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            delta(
                0,
                json!({"type": "thinking_delta", "thinking": "secret reasoning"}),
            ),
            delta(0, json!({"type": "signature_delta", "signature": "EqQB"})),
            stop(0),
            start(
                1,
                json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}),
            ),
            stop(1),
            start(2, json!({"type": "text", "text": ""})),
            delta(2, json!({"type": "text_delta", "text": "answer"})),
            stop(2),
            message_delta("end_turn"),
            message_stop(),
        ] {
            assembler.feed(&data).unwrap();
        }
        let response = assembler.finish(HashSet::new()).unwrap();

        // The thinking block is provider-visible protocol data, not visible content and not
        // the reasoning channel (ADR 0014 R10).
        assert_eq!(response.content, vec![ContentPart::text("answer")]);
        assert!(response.reasoning.is_empty());
        assert!(response.tool_calls.is_empty());
        let payload = response
            .provider_payload
            .expect("the thinking block is kept");
        let payload: Value = serde_json::from_str(payload.get()).unwrap();
        assert_eq!(
            payload,
            json!([{"type": "thinking", "thinking": "secret reasoning", "signature": "EqQB"}])
        );
        // No thinking text reached the UI: only the visible text delta was emitted.
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            [AgentEvent::ModelTextDelta {
                text: "answer".into()
            }]
        );
    }

    #[test]
    fn redacted_thinking_blocks_round_trip_verbatim() {
        let response = assemble(&[
            message_start(None),
            start(
                0,
                json!({"type": "redacted_thinking", "data": "EmgBAQ...opaque"}),
            ),
            stop(0),
            start(1, json!({"type": "text", "text": ""})),
            delta(1, json!({"type": "text_delta", "text": "answer"})),
            stop(1),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        let payload = response.provider_payload.expect("redacted block kept");
        let payload: Value = serde_json::from_str(payload.get()).unwrap();
        assert_eq!(
            payload,
            json!([{"type": "redacted_thinking", "data": "EmgBAQ...opaque"}])
        );
    }

    #[test]
    fn thinking_blocks_keep_their_streamed_order() {
        let response = assemble(&[
            message_start(None),
            start(
                0,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            delta(0, json!({"type": "thinking_delta", "thinking": "one"})),
            delta(
                0,
                json!({"type": "signature_delta", "signature": "sig-one"}),
            ),
            stop(0),
            start(1, json!({"type": "redacted_thinking", "data": "opaque"})),
            stop(1),
            start(
                2,
                json!({"type": "thinking", "thinking": "", "signature": ""}),
            ),
            delta(2, json!({"type": "thinking_delta", "thinking": "two"})),
            delta(
                2,
                json!({"type": "signature_delta", "signature": "sig-two"}),
            ),
            stop(2),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        let payload: Value =
            serde_json::from_str(response.provider_payload.unwrap().get()).unwrap();
        assert_eq!(
            payload,
            json!([
                {"type": "thinking", "thinking": "one", "signature": "sig-one"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "thinking", "thinking": "two", "signature": "sig-two"}
            ])
        );
    }

    #[test]
    fn a_response_without_thinking_has_no_provider_payload() {
        let response = assemble(&[
            message_start(None),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "plain"})),
            stop(0),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        assert!(response.provider_payload.is_none());
    }

    #[test]
    fn missing_ids_are_made_up_and_repeated_ids_are_renamed() {
        let taken: HashSet<String> = ["toolu_0".to_owned()].into();
        let response = assemble_with(
            &[
                message_start(None),
                start(0, json!({"type": "tool_use", "name": "a", "input": {}})),
                stop(0),
                start(
                    1,
                    json!({"type": "tool_use", "id": "dup", "name": "b", "input": {}}),
                ),
                stop(1),
                start(
                    2,
                    json!({"type": "tool_use", "id": "dup", "name": "c", "input": {}}),
                ),
                stop(2),
                message_delta("tool_use"),
                message_stop(),
            ],
            taken,
            usize::MAX,
        )
        .unwrap();
        let ids: Vec<_> = response.tool_calls.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids, ["toolu_0-2", "dup", "dup-2"]);
    }

    #[test]
    fn stop_reasons_map_to_stop_reasons() {
        for (raw, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("pause_turn", StopReason::PauseTurn),
            ("refusal", StopReason::Refusal),
            ("stop_sequence", StopReason::EndTurn),
            ("model_context_window_exceeded", StopReason::MaxTokens),
            ("something_new", StopReason::Other("something_new".into())),
        ] {
            let response = assemble(&[
                message_start(None),
                start(0, json!({"type": "text", "text": ""})),
                delta(0, json!({"type": "text_delta", "text": "x"})),
                stop(0),
                message_delta(raw),
                message_stop(),
            ])
            .unwrap();
            assert_eq!(response.stop_reason, expected, "{raw}");
        }
    }

    #[test]
    fn a_stream_without_message_stop_is_an_error() {
        let error = assemble(&[
            message_start(None),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "half an ans"})),
            message_delta("end_turn"),
        ])
        .err()
        .unwrap();
        assert!(matches!(error, ModelError::Transport(_)), "{error}");
    }

    #[test]
    fn message_stop_without_a_stop_reason_is_an_error() {
        let error = assemble(&[
            message_start(None),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "x"})),
            message_stop(),
        ])
        .err()
        .unwrap();
        assert!(matches!(error, ModelError::Transport(_)), "{error}");
    }

    #[test]
    fn an_error_event_ends_the_response() {
        let error = assemble(&[
            message_start(None),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})
                .to_string(),
        ])
        .err()
        .unwrap();
        assert!(
            matches!(&error, ModelError::Transport(m) if m.contains("Overloaded")),
            "{error}"
        );
    }

    #[test]
    fn ping_and_unknown_events_are_ignored() {
        let response = assemble(&[
            message_start(None),
            json!({"type": "ping"}).to_string(),
            json!({"type": "future_event", "x": 1}).to_string(),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "ok"})),
            stop(0),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        assert_eq!(response.content, vec![ContentPart::text("ok")]);
    }

    #[test]
    fn usage_merges_message_start_and_message_delta_field_by_field() {
        let response = assemble(&[
            message_start(Some(
                json!({"input_tokens": 472, "cache_read_input_tokens": 3, "output_tokens": 2}),
            )),
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "ok"})),
            stop(0),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"output_tokens": 89, "cache_creation_input_tokens": 7}})
            .to_string(),
            message_stop(),
        ])
        .unwrap();
        let usage = response.usage.expect("usage was reported");
        assert_eq!(usage.input, Some(472));
        assert_eq!(usage.output, Some(89));
        assert_eq!(usage.cache_read, Some(3));
        assert_eq!(usage.cache_write, Some(7));
        assert_eq!(usage.total, None);
        // Anthropic's `input_tokens` is the uncached part (ADR 0007): `input` is
        // billed in full, the cache fields at their own rates, and the prompt the provider saw
        // is all three added up.
        assert_eq!(usage.input_includes_cache, Some(false));
        assert_eq!(usage.uncached_input(), Some(472));
        assert_eq!(usage.prompt_total(), Some(482));
    }

    #[test]
    fn usage_takes_the_last_non_null_value_per_field() {
        let response = assemble(&[
            message_start(Some(json!({"input_tokens": 1}))),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                   "usage": {"input_tokens": 9, "output_tokens": 2}})
            .to_string(),
            message_stop(),
        ])
        .unwrap();
        let usage = response.usage.expect("usage was reported");
        assert_eq!(usage.input, Some(9));
        assert_eq!(usage.output, Some(2));
    }

    #[test]
    fn usage_details_keep_the_provider_sub_object() {
        let response = assemble(&[
            message_start(Some(
                json!({"input_tokens": 1, "output_tokens_details": {"thinking_tokens": 5}}),
            )),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        let details = response.usage.unwrap().details.expect("details are kept");
        let parsed: Value = serde_json::from_str(details.get()).unwrap();
        assert_eq!(parsed, json!({"thinking_tokens": 5}));
    }

    #[test]
    fn no_usage_means_none() {
        let response = assemble(&[
            message_start(None),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        assert_eq!(response.usage, None);
    }

    #[test]
    fn total_is_never_invented() {
        let response = assemble(&[
            message_start(Some(json!({"input_tokens": 7, "output_tokens": 2}))),
            message_delta("end_turn"),
            message_stop(),
        ])
        .unwrap();
        let usage = response.usage.unwrap();
        assert_eq!(usage.input, Some(7));
        assert_eq!(usage.output, Some(2));
        assert_eq!(usage.total, None, "total must stay None");
    }

    #[test]
    fn a_response_over_the_limit_is_cut_off() {
        let mut events = vec![
            message_start(None),
            start(0, json!({"type": "text", "text": ""})),
        ];
        events
            .extend((0..10).map(|_| delta(0, json!({"type": "text_delta", "text": "0123456789"}))));
        events.push(message_delta("end_turn"));
        events.push(message_stop());
        let error = assemble_with(&events, HashSet::new(), 50).err().unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");
    }

    #[test]
    fn garbage_is_a_protocol_error() {
        let error = assemble(&["not json".into()]).err().unwrap();
        assert!(matches!(error, ModelError::Protocol(_)), "{error}");
    }
}
