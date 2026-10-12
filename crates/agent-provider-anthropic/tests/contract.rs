//! The Anthropic adapter against the adapter-neutral contract suite.
//!
//! Wire-level details (event sequences, deltas, headers, timeouts) stay in `http.rs`; this
//! file implements the [`ContractHarness`] translation of semantic wires to Anthropic
//! Messages events and nothing else.
//!
//! Anthropic has no `content_filter` stop reason (it uses `refusal`, which the suite checks
//! separately), so the harness **explicitly declares** `Reason::ContentFiltered` unsupported
//! rather than inventing a value Anthropic never sends. See
//! `docs/providers/anthropic-wire-notes.md` §4.4/§5.

use std::sync::Mutex;

use agent_core::*;
use agent_provider_anthropic::testing::{
    MockServer, Reply, block_stop, error, input_json_delta, message_delta, message_start,
    message_stop, text_delta, text_start, tool_start,
};
use agent_provider_anthropic::{Anthropic, AnthropicConfig, ApiKey};
use agent_provider_contract::{FnCall, Reason, UsageField, Wire};
use serde_json::{Map, Value, json};

const SECRET: &str = "sk-ant-contract-secret-token";

struct AnthropicHarness {
    /// The mock servers stay alive here for the duration of a check.
    servers: Mutex<Vec<MockServer>>,
}

impl Default for AnthropicHarness {
    fn default() -> Self {
        Self {
            servers: Mutex::new(Vec::new()),
        }
    }
}

impl agent_provider_contract::ContractHarness<Anthropic> for AnthropicHarness {
    fn secret(&self) -> &str {
        SECRET
    }

    async fn script(&self, wires: &[Wire]) -> Anthropic {
        let replies: Vec<Reply> = wires.iter().map(|w| self.wire_to_reply(w)).collect();
        let server = MockServer::start(replies).await;
        let base = server.base_url.clone();
        self.servers.lock().unwrap().push(server);
        let config = AnthropicConfig::new(base, "contract-model").with_api_key(ApiKey::new(SECRET));
        Anthropic::new(config).expect("a local mock config is valid")
    }

    fn unsupported_reasons(&self) -> &'static [Reason] {
        // Anthropic's stop_reason set is exactly the seven values in the wire notes §4.4;
        // there is no content-filter value, so the suite must not require one.
        &[Reason::ContentFiltered]
    }

    fn unreported_usage_fields(&self) -> &'static [UsageField] {
        // Anthropic's usage object has no total field (wire notes §3), so the adapter must
        // leave `total` None; the contract asserts that absence rather than skipping it.
        &[UsageField::Total]
    }
}

impl AnthropicHarness {
    fn wire_to_reply(&self, wire: &Wire) -> Reply {
        match wire {
            Wire::Text(text) => Reply::sse(&[
                message_start(None),
                text_start(0),
                text_delta(0, text),
                block_stop(0),
                message_delta("end_turn", None),
                message_stop(),
            ]),
            Wire::TextInPieces(pieces) => {
                let mut events = vec![message_start(None), text_start(0)];
                events.extend(pieces.iter().map(|piece| text_delta(0, piece)));
                events.push(block_stop(0));
                events.push(message_delta("end_turn", None));
                events.push(message_stop());
                Reply::sse(&events)
            }
            Wire::ToolCall(call) => Reply::sse(&tool_events(call)),
            Wire::ToolCalls(calls) => {
                let mut events = vec![message_start(None)];
                for (index, call) in calls.iter().enumerate() {
                    events.push(tool_start(index, Some(&call.id), &call.name, json!({})));
                    events.push(input_json_delta(index, &arguments_string(&call.arguments)));
                    events.push(block_stop(index));
                }
                events.push(message_delta("tool_use", None));
                events.push(message_stop());
                Reply::sse(&events)
            }
            Wire::ToolCallSplitArguments { id, name, pieces } => {
                let mut events = vec![
                    message_start(None),
                    tool_start(0, Some(id), name, json!({})),
                ];
                events.extend(pieces.iter().map(|piece| input_json_delta(0, piece)));
                events.push(block_stop(0));
                events.push(message_delta("tool_use", None));
                events.push(message_stop());
                Reply::sse(&events)
            }
            Wire::ToolCallNoId { name, arguments } => Reply::sse(&[
                message_start(None),
                tool_start(0, None, name, json!({})),
                input_json_delta(0, &arguments_string(arguments)),
                block_stop(0),
                message_delta("tool_use", None),
                message_stop(),
            ]),
            Wire::Finish(reason) => Reply::sse(&[
                message_start(None),
                text_start(0),
                text_delta(0, "x"),
                block_stop(0),
                message_delta(raw_reason(*reason), None),
                message_stop(),
            ]),
            Wire::Refusal(text) => Reply::sse(&[
                message_start(None),
                text_start(0),
                text_delta(0, text),
                block_stop(0),
                message_delta("refusal", None),
                message_stop(),
            ]),
            Wire::UsageTrailing(usage) => Reply::sse(&[
                message_start(None),
                text_start(0),
                text_delta(0, "ok"),
                block_stop(0),
                message_delta("end_turn", Some(usage_json(usage))),
                message_stop(),
            ]),
            // Anthropic carries usage on `message_start` and `message_delta`; the last value
            // for each field wins, which is exactly what the check asserts.
            Wire::UsageEveryChunk(usages) => {
                let (first, rest) = usages.split_first().expect("the check scripts two usages");
                let mut events = vec![message_start(Some(usage_json(first)))];
                events.push(text_start(0));
                events.push(text_delta(0, "ok"));
                events.push(block_stop(0));
                for usage in rest {
                    events.push(message_delta("end_turn", Some(usage_json(usage))));
                }
                events.push(message_stop());
                Reply::sse(&events)
            }
            Wire::NoUsage => Reply::sse(&[
                message_start(None),
                text_start(0),
                text_delta(0, "ok"),
                block_stop(0),
                message_delta("end_turn", None),
                message_stop(),
            ]),
            Wire::ErrorEchoesSecret => Reply::sse(&[
                message_start(None),
                error(&format!("rejected key {SECRET}")),
            ]),
            Wire::Status { code, body } => Reply::status(*code, body),
            Wire::CutMidStream(text) => Reply::SseThenCut {
                pieces: vec![
                    format!("data: {}\n\n", message_start(None)).into_bytes(),
                    format!("data: {}\n\n", text_start(0)).into_bytes(),
                    format!("data: {}\n\n", text_delta(0, text)).into_bytes(),
                ],
            },
            Wire::Hangs => Reply::SseThenHang { pieces: Vec::new() },
        }
    }
}

fn tool_events(call: &FnCall) -> Vec<Value> {
    vec![
        message_start(None),
        tool_start(0, Some(&call.id), &call.name, json!({})),
        input_json_delta(0, &arguments_string(&call.arguments)),
        block_stop(0),
        message_delta("tool_use", None),
        message_stop(),
    ]
}

/// `Reason::ContentFiltered` is declared unsupported, so this branch is never reached by the
/// suite; the value is Anthropic-shaped only to keep the translation total. It is **not** a
/// claim that Anthropic sends `content_filter`.
fn raw_reason(reason: Reason) -> &'static str {
    match reason {
        Reason::EndTurn => "end_turn",
        Reason::ToolUse => "tool_use",
        Reason::MaxTokens => "max_tokens",
        Reason::ContentFiltered => "content_filter",
        Reason::PauseTurn => "pause_turn",
        Reason::Unknown => "something_new",
    }
}

fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// A neutral `ModelUsage` back to Anthropic's usage JSON. Anthropic has no `total` field
/// (wire notes §3), so the harness drops it; the adapter declares `total` unreported and the
/// contract asserts it stays `None` (rather than skipping the field).
fn usage_json(usage: &ModelUsage) -> Value {
    let mut body = Map::new();
    if let Some(value) = usage.input {
        body.insert("input_tokens".into(), json!(value));
    }
    if let Some(value) = usage.output {
        body.insert("output_tokens".into(), json!(value));
    }
    if let Some(value) = usage.cache_read {
        body.insert("cache_read_input_tokens".into(), json!(value));
    }
    if let Some(value) = usage.cache_write {
        body.insert("cache_creation_input_tokens".into(), json!(value));
    }
    if let Some(raw) = &usage.details {
        let parsed: Value = serde_json::from_str(raw.get()).expect("details is valid JSON");
        body.insert("output_tokens_details".into(), parsed);
    }
    Value::Object(body)
}

fn harness() -> AnthropicHarness {
    AnthropicHarness::default()
}

use agent_provider_contract as contract;

#[tokio::test]
async fn contract_1_plain_text() {
    contract::check_plain_text(&harness()).await
}

#[tokio::test]
async fn contract_2_streamed_deltas() {
    contract::check_stream_deltas(&harness()).await
}

#[tokio::test]
async fn contract_3a_single_tool_call() {
    contract::check_single_tool_call(&harness()).await
}

#[tokio::test]
async fn contract_3b_multiple_tool_calls() {
    contract::check_multiple_tool_calls(&harness()).await
}

#[tokio::test]
async fn contract_4_split_arguments() {
    contract::check_split_arguments(&harness()).await
}

#[tokio::test]
async fn contract_5_call_ids_unique() {
    contract::check_call_ids_unique(&harness()).await
}

#[tokio::test]
async fn contract_6_finish_reasons() {
    contract::check_finish_reasons(&harness()).await
}

#[tokio::test]
async fn contract_7a_usage_shapes() {
    contract::check_usage_shapes(&harness()).await
}

#[tokio::test]
async fn contract_7b_total_not_computed() {
    contract::check_total_not_computed(&harness()).await
}

#[tokio::test]
async fn contract_8_error_classification() {
    contract::check_error_classification(&harness()).await
}

#[tokio::test]
async fn contract_9_cancellation() {
    contract::check_cancellation(&harness()).await
}

#[tokio::test]
async fn contract_10_secret_not_leaked() {
    contract::check_secret_not_leaked(&harness()).await
}
