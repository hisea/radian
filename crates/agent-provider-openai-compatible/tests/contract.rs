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

    fn context_overflow(&self) -> Option<Wire> {
        // OpenAI's real 400 shape for an over-length prompt (Completions/Responses API): the
        // structured code `context_length_exceeded` plus the "maximum context length" message.
        Some(Wire::Status {
            code: 400,
            body: json!({
                "error": {
                    "message": "This model's maximum context length is 16385 tokens. However, your messages resulted in 20000 tokens.",
                    "type": "invalid_request_error",
                    "code": "context_length_exceeded"
                }
            })
            .to_string(),
        })
    }

    fn input_includes_cache(&self) -> bool {
        // OpenAI's `prompt_tokens` contains `prompt_tokens_details.cached_tokens`
        // (`total_tokens = prompt_tokens + completion_tokens`), so the cached tokens are
        // already inside the input count (ADR 0007).
        true
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
/// `completion_tokens`, `total_tokens`, and the `*_details` sub-objects). The neutral
/// `details` blob is keyed by those sub-object names, so each key maps to its wire key and
/// both prompt and completion details survive the round trip.
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
    let mut prompt_details = Map::new();
    if let Some(v) = usage.cache_read {
        prompt_details.insert("cached_tokens".into(), json!(v));
    }
    let mut completion_details = Map::new();
    if let Some(raw) = &usage.details {
        let parsed: Value = serde_json::from_str(raw.get()).expect("details is valid JSON");
        if let Value::Object(subs) = parsed {
            for (key, value) in subs {
                let Value::Object(fields) = value else {
                    continue;
                };
                match key.as_str() {
                    "prompt_tokens_details" => prompt_details.extend(fields),
                    "completion_tokens_details" => completion_details.extend(fields),
                    _ => {}
                }
            }
        }
    }
    if !prompt_details.is_empty() {
        body.insert(
            "prompt_tokens_details".into(),
            Value::Object(prompt_details),
        );
    }
    if !completion_details.is_empty() {
        body.insert(
            "completion_tokens_details".into(),
            Value::Object(completion_details),
        );
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

/// ADR 0007: the adapter says whether `input` contains the cache hit. This one
/// declares `true` — OpenAI's `prompt_tokens` contains
/// `prompt_tokens_details.cached_tokens`.
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

/// ADR 0018: a user message may carry an attached file as its **own** part. The
/// OpenAI-compatible adapter joins the parts with a blank line, exactly as before, and there
/// is no `<file>` wrapper any more.
#[tokio::test]
async fn contract_12_attachment_parts_are_joined_by_blank_lines() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let base = server.base_url.clone();
    let model = OpenAiCompatible::new(
        OpenAiCompatibleConfig::new(base, "contract-model").with_api_key(ApiKey::new(SECRET)),
    )
    .expect("a local mock config is valid");
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::User {
            content: vec![
                ContentPart::text("look at @notes.txt"),
                ContentPart::attachment("alpha\nbeta", "notes.txt"),
            ],
            meta: MessageMeta::default(),
        }],
        tools: Vec::new(),
        settings: Value::Null,
    };
    model
        .complete(request, agent_provider_contract::context(&NoopSink))
        .await
        .unwrap();
    let body = server.requests()[0].json();
    let content = body["messages"][0]["content"]
        .as_str()
        .expect("the user content is one string on this wire");
    assert_eq!(content, "look at @notes.txt\n\nalpha\nbeta");
}

/// ADR 0018, decision 1: the request's `settings` win over the adapter config, so
/// the summary call can run at its own (lower) reasoning level. The adapter passes through the
/// value it is given verbatim and invents nothing.
#[tokio::test]
async fn contract_13_the_request_reasoning_effort_wins_over_the_config() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "ok"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;
    let base = server.base_url.clone();
    let mut config = OpenAiCompatibleConfig::new(base, "contract-model")
        .with_api_key(ApiKey::new(SECRET));
    config.reasoning_effort = Some("high".into());
    let model = OpenAiCompatible::new(config).expect("a local mock config is valid");
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: json!({"reasoning_effort": "low"}),
    };
    model
        .complete(request, agent_provider_contract::context(&NoopSink))
        .await
        .unwrap();
    let body = server.requests()[0].json();
    assert_eq!(
        body["reasoning_effort"], "low",
        "the request setting must win over the configured `high`"
    );
}
