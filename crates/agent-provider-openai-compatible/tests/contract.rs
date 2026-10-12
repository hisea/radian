//! The OpenAI-compatible adapter against the adapter-neutral contract suite.
//!
//! Wire-level details (SSE splitting, headers, timeouts, exact chunk shapes) stay in
//! `http.rs`; this file implements the [`ContractHarness`] translation of semantic wires to
//! OpenAI SSE chunks and nothing else.

use std::sync::Mutex;

use agent_core::*;
use agent_provider_contract::{FnCall, Reason, Wire};
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use agent_provider_openai_compatible::{ApiKey, OpenAiCompatible, OpenAiCompatibleConfig};
use serde_json::{Map, Value, json};

const SECRET: &str = "sk-contract-secret-token";

struct OpenAiHarness {
    /// The mock servers stay alive here for the duration of a check.
    servers: Mutex<Vec<MockServer>>,
}

impl Default for OpenAiHarness {
    fn default() -> Self {
        Self {
            servers: Mutex::new(Vec::new()),
        }
    }
}

impl agent_provider_contract::ContractHarness<OpenAiCompatible> for OpenAiHarness {
    fn secret(&self) -> &str {
        SECRET
    }

    async fn script(&self, wires: &[Wire]) -> OpenAiCompatible {
        let replies: Vec<Reply> = wires.iter().map(|w| self.wire_to_reply(w)).collect();
        let server = MockServer::start(replies).await;
        let base = server.base_url.clone();
        self.servers.lock().unwrap().push(server);
        let config =
            OpenAiCompatibleConfig::new(base, "contract-model").with_api_key(ApiKey::new(SECRET));
        OpenAiCompatible::new(config).expect("a local mock config is valid")
    }
}

impl OpenAiHarness {
    fn wire_to_reply(&self, wire: &Wire) -> Reply {
        match wire {
            Wire::Text(text) => Reply::sse(&[
                chunk(json!({ "role": "assistant", "content": text }), None),
                chunk(json!({}), Some("stop")),
            ]),
            Wire::TextInPieces(pieces) => {
                let mut chunks: Vec<Value> = pieces
                    .iter()
                    .map(|p| chunk(json!({ "content": p }), None))
                    .collect();
                chunks.push(chunk(json!({}), Some("stop")));
                Reply::sse(&chunks)
            }
            Wire::ToolCall(call) => {
                Reply::sse(&[call_chunk(call), chunk(json!({}), Some("tool_calls"))])
            }
            Wire::ToolCalls(calls) => {
                let calls: Vec<Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(index, call)| {
                        json!({
                            "index": index,
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": arguments_string(&call.arguments),
                            },
                        })
                    })
                    .collect();
                Reply::sse(&[
                    chunk(json!({ "tool_calls": calls }), None),
                    chunk(json!({}), Some("tool_calls")),
                ])
            }
            Wire::ToolCallSplitArguments { id, name, pieces } => {
                let mut chunks = Vec::new();
                for (i, piece) in pieces.iter().enumerate() {
                    if i == 0 {
                        chunks.push(chunk(
                            json!({
                                "tool_calls": [{
                                    "index": 0, "id": id, "type": "function",
                                    "function": { "name": name, "arguments": piece },
                                }]
                            }),
                            None,
                        ));
                    } else {
                        chunks.push(chunk(
                            json!({
                                "tool_calls": [{
                                    "index": 0,
                                    "function": { "arguments": piece },
                                }]
                            }),
                            None,
                        ));
                    }
                }
                chunks.push(chunk(json!({}), Some("tool_calls")));
                Reply::sse(&chunks)
            }
            Wire::ToolCallNoId { name, arguments } => Reply::sse(&[
                chunk(
                    json!({
                        "tool_calls": [{
                            "index": 0, "type": "function",
                            "function": { "name": name, "arguments": arguments_string(arguments) },
                        }]
                    }),
                    None,
                ),
                chunk(json!({}), Some("tool_calls")),
            ]),
            Wire::Finish(reason) => {
                Reply::sse(&[chunk(json!({ "content": "x" }), Some(raw_reason(*reason)))])
            }
            Wire::Refusal(text) => Reply::sse(&[
                chunk(json!({ "refusal": text }), None),
                chunk(json!({}), Some("stop")),
            ]),
            Wire::UsageTrailing(usage) => Reply::sse(&[
                chunk(json!({ "content": "ok" }), Some("stop")),
                json!({ "choices": [], "usage": usage_json(usage) }),
            ]),
            Wire::UsageEveryChunk(usages) => {
                let mut chunks: Vec<Value> = usages
                    .iter()
                    .map(|u| json!({ "choices": [], "usage": usage_json(u) }))
                    .collect();
                chunks.push(chunk(json!({ "content": "ok" }), Some("stop")));
                Reply::sse(&chunks)
            }
            Wire::NoUsage => Reply::sse(&[
                chunk(json!({ "role": "assistant", "content": "ok" }), None),
                chunk(json!({}), Some("stop")),
            ]),
            Wire::ErrorEchoesSecret => Reply::sse(&[json!({
                "error": { "message": format!("rejected key {SECRET}") }
            })]),
            Wire::Status { code, body } => Reply::status(*code, body),
            Wire::CutMidStream(text) => Reply::SseThenCut {
                pieces: vec![
                    format!("data: {}\n\n", chunk(json!({ "content": text }), None)).into_bytes(),
                ],
            },
            Wire::Hangs => Reply::SseThenHang { pieces: Vec::new() },
        }
    }
}

fn raw_reason(reason: Reason) -> &'static str {
    match reason {
        Reason::EndTurn => "stop",
        Reason::ToolUse => "tool_calls",
        Reason::MaxTokens => "length",
        Reason::ContentFiltered => "content_filter",
        Reason::PauseTurn => "pause_turn",
        Reason::Unknown => "something_new",
    }
}

fn call_chunk(call: &FnCall) -> Value {
    chunk(
        json!({
            "tool_calls": [{
                "index": 0,
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": arguments_string(&call.arguments) },
            }]
        }),
        None,
    )
}

fn arguments_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}

/// A neutral `ModelUsage` back to OpenAI's usage JSON (`prompt_tokens`,
/// `completion_tokens`, `total_tokens`, `prompt_tokens_details`).
fn usage_json(usage: &ModelUsage) -> Value {
    let mut body = Map::new();
    if let Some(v) = usage.input {
        body.insert("prompt_tokens".into(), json!(v));
    }
    if let Some(v) = usage.output {
        body.insert("completion_tokens".into(), json!(v));
    }
    if let Some(v) = usage.total {
        body.insert("total_tokens".into(), json!(v));
    }
    if let Some(v) = usage.cache_write {
        body.insert("cache_creation_input_tokens".into(), json!(v));
    }
    let mut details = Map::new();
    if let Some(v) = usage.cache_read {
        details.insert("cached_tokens".into(), json!(v));
    }
    if let Some(raw) = &usage.details {
        let parsed: Value = serde_json::from_str(raw.get()).expect("details is valid JSON");
        if let Value::Object(extra) = parsed {
            for (k, v) in extra {
                details.insert(k, v);
            }
        }
    }
    if !details.is_empty() {
        body.insert("prompt_tokens_details".into(), Value::Object(details));
    }
    Value::Object(body)
}

fn harness() -> OpenAiHarness {
    OpenAiHarness::default()
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
