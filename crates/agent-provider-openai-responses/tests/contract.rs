//! The OpenAI Responses adapter against the adapter-neutral contract suite.
//!
//! Wire-level details (exact event shapes, headers, timeouts) stay in `http.rs`; this file
//! implements the [`ContractHarness`] translation of semantic wires to Responses SSE events
//! and nothing else.
//!
//! The Responses protocol can express every contract reason: `completed`/`end_turn`/`incomplete`
//! cover the finish reasons, a refusal is a `refusal` content part, and a context filter is
//! `incomplete_details.reason == "content_filter"`. So no reason is declared unsupported.

use std::sync::Mutex;

use agent_provider_contract::{FnCall, Reason, Wire};
use agent_provider_openai_responses::testing::{
    MockServer, Reply, completed, completed_pausing, created, function_call_added,
    function_call_arguments_delta, function_call_arguments_done, incomplete, message_item,
    output_item_done, refusal_item, text_delta,
};
use agent_provider_openai_responses::{OpenAiResponses, OpenAiResponsesConfig};
use radian_core::*;
use serde_json::{Map, Value, json};

const SECRET: &str = "sk-contract-secret-token";

struct ResponsesHarness {
    /// The mock servers stay alive here for the duration of a check.
    servers: Mutex<Vec<MockServer>>,
}

impl Default for ResponsesHarness {
    fn default() -> Self {
        Self {
            servers: Mutex::new(Vec::new()),
        }
    }
}

impl agent_provider_contract::ContractHarness<OpenAiResponses> for ResponsesHarness {
    fn secret(&self) -> &str {
        SECRET
    }

    async fn script(&self, wires: &[Wire]) -> OpenAiResponses {
        let replies: Vec<Reply> = wires.iter().map(|w| self.wire_to_reply(w)).collect();
        let server = MockServer::start(replies).await;
        let base = server.base_url.clone();
        self.servers.lock().unwrap().push(server);
        let config = OpenAiResponsesConfig::new(base, "contract-model")
            .with_header("Authorization", format!("Bearer {SECRET}"))
            .with_header("chatgpt-account-id", "acct-contract")
            .with_header("OpenAI-Beta", "responses=experimental")
            .with_header("originator", "radian");
        OpenAiResponses::new(config).expect("a local mock config is valid")
    }

    fn input_includes_cache(&self) -> bool {
        // Responses' `input_tokens` contains `input_tokens_details.cached_tokens`
        // (ADR 0007).
        true
    }

    fn context_overflow(&self) -> Option<Wire> {
        // OpenAI's real 400 shape for an over-length input.
        Some(Wire::Status {
            code: 400,
            body: json!({
                "error": {
                    "message": "This model's maximum context length is 16385 tokens. However, \
                                your input resulted in 20000 tokens.",
                    "type": "invalid_request_error",
                    "code": "context_length_exceeded"
                }
            })
            .to_string(),
        })
    }
}

impl ResponsesHarness {
    fn wire_to_reply(&self, wire: &Wire) -> Reply {
        match wire {
            Wire::Text(text) => Reply::sse(&[
                text_delta(0, text),
                output_item_done(0, message_item("msg_1", text)),
                completed(None),
            ]),
            Wire::TextInPieces(pieces) => {
                let mut events: Vec<Value> = pieces.iter().map(|p| text_delta(0, p)).collect();
                events.push(output_item_done(0, message_item("msg_1", &pieces.concat())));
                events.push(completed(None));
                Reply::sse(&events)
            }
            Wire::ToolCall(call) => Reply::sse(&single_call_events(call)),
            Wire::ToolCalls(calls) => {
                let mut events = Vec::new();
                for (index, call) in calls.iter().enumerate() {
                    events.push(function_call_added(
                        index,
                        &format!("fc_{index}"),
                        &call.id,
                        &call.name,
                    ));
                    events.push(function_call_arguments_done(
                        index,
                        &format!("fc_{index}"),
                        &arguments_string(&call.arguments),
                    ));
                }
                events.push(completed(None));
                Reply::sse(&events)
            }
            // The arguments arrive only as deltas (no authoritative `done`), so the check
            // exercises the split-assembly path.
            Wire::ToolCallSplitArguments { id, name, pieces } => {
                let mut events = vec![function_call_added(0, "fc_1", id, name)];
                events.extend(
                    pieces
                        .iter()
                        .map(|piece| function_call_arguments_delta(0, "fc_1", piece)),
                );
                events.push(completed(None));
                Reply::sse(&events)
            }
            Wire::ToolCallNoId { name, arguments } => Reply::sse(&[
                // No `call_id` and no `id`: the adapter must make one up.
                output_item_added_no_id(name),
                function_call_arguments_done(0, "", &arguments_string(arguments)),
                completed(None),
            ]),
            Wire::Finish(reason) => Reply::sse(&finish_events(*reason)),
            Wire::Refusal(text) => Reply::sse(&[
                json!({"type": "response.refusal.delta", "output_index": 0, "delta": text}),
                output_item_done(0, refusal_item("msg_1", text)),
                completed(None),
            ]),
            Wire::UsageTrailing(usage) => Reply::sse(&[
                output_item_done(0, message_item("msg_1", "ok")),
                completed(Some(usage_json(usage))),
            ]),
            Wire::UsageEveryChunk(usages) => {
                let (first, rest) = usages.split_first().expect("the check scripts two usages");
                let mut events = vec![created(Some(usage_json(first)))];
                let last = rest.last().unwrap_or(first);
                events.push(completed(Some(usage_json(last))));
                Reply::sse(&events)
            }
            Wire::NoUsage => Reply::sse(&[completed(None)]),
            Wire::ErrorEchoesSecret => Reply::sse(&[json!({"type": "error", "error": {
                "code": "invalid_api_key",
                "message": format!("rejected key {SECRET}")
            }})]),
            Wire::Status { code, body } => Reply::status(*code, body),
            Wire::CutMidStream(text) => Reply::SseThenCut {
                pieces: vec![format!("data: {}\n\n", text_delta(0, text)).into_bytes()],
            },
            Wire::Hangs => Reply::SseThenHang { pieces: Vec::new() },
        }
    }
}

fn single_call_events(call: &FnCall) -> Vec<Value> {
    vec![
        function_call_added(0, "fc_1", &call.id, &call.name),
        function_call_arguments_done(0, "fc_1", &arguments_string(&call.arguments)),
        completed(None),
    ]
}

fn output_item_added_no_id(name: &str) -> Value {
    json!({"type": "response.output_item.added", "output_index": 0,
           "item": {"type": "function_call", "name": name, "arguments": ""}})
}

fn finish_events(reason: Reason) -> Vec<Value> {
    match reason {
        Reason::EndTurn => vec![completed(None)],
        Reason::ToolUse => single_call_events(&FnCall {
            id: "tc_1".into(),
            name: "f".into(),
            arguments: json!({}),
        }),
        Reason::MaxTokens => vec![incomplete("max_output_tokens", None)],
        Reason::ContentFiltered => vec![incomplete("content_filter", None)],
        Reason::PauseTurn => vec![completed_pausing(None)],
        Reason::Unknown => vec![json!({"type": "response.completed",
            "response": {"id": "resp_test", "status": "something_new", "usage": null}})],
    }
}

fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// A neutral `ModelUsage` back to the Responses usage object (`input_tokens`,
/// `input_tokens_details`, `output_tokens`, `output_tokens_details`, `total_tokens`). The
/// neutral `details` blob uses the ADR 0007 J3 keys (`prompt_tokens_details` /
/// `completion_tokens_details`), which map to the Responses `input_tokens_details` /
/// `output_tokens_details` sub-objects.
fn usage_json(usage: &ModelUsage) -> Value {
    let mut body = Map::new();
    if let Some(value) = usage.input {
        body.insert("input_tokens".into(), json!(value));
    }
    if let Some(value) = usage.output {
        body.insert("output_tokens".into(), json!(value));
    }
    if let Some(value) = usage.total {
        body.insert("total_tokens".into(), json!(value));
    }
    let mut input_details = Map::new();
    let mut output_details = Map::new();
    if let Some(raw) = &usage.details {
        let parsed: Value = serde_json::from_str(raw.get()).expect("details is valid JSON");
        if let Some(sub) = parsed
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
        {
            input_details.extend(sub.clone());
        }
        if let Some(sub) = parsed
            .get("completion_tokens_details")
            .and_then(Value::as_object)
        {
            output_details.extend(sub.clone());
        }
    }
    if let Some(value) = usage.cache_read {
        input_details.insert("cached_tokens".into(), json!(value));
    }
    if let Some(value) = usage.cache_write {
        input_details.insert("cache_write_tokens".into(), json!(value));
    }
    if !input_details.is_empty() {
        body.insert("input_tokens_details".into(), Value::Object(input_details));
    }
    if !output_details.is_empty() {
        body.insert(
            "output_tokens_details".into(),
            Value::Object(output_details),
        );
    }
    Value::Object(body)
}

fn harness() -> ResponsesHarness {
    ResponsesHarness::default()
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

/// ADR 0007: Responses' `input_tokens` contains the cache hit.
#[tokio::test]
async fn contract_7c_input_cache_semantics() {
    contract::check_input_cache_semantics(&harness()).await
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

#[tokio::test]
async fn contract_11_context_overflow() {
    contract::check_context_overflow(&harness()).await
}
