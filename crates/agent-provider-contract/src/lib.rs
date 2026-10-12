//! Adapter-neutral provider contract suite.
//!
//! Phase 0a's completion criterion asks for scripted/HTTP fixture contract tests per
//! provider. This crate pins what **any** [`Model`] implementation must do, described in
//! semantic terms (a [`Wire`]), and leaves the translation to each provider's line format to
//! the adapter itself (a [`ContractHarness`] implementation). Phase 3's Anthropic adapter
//! implements the same harness against its event stream and reuses every check.
//!
//! The checks drive [`Model::complete`] directly (not through `Agent`): the contract is
//! about the adapter, and the round trip through the turn loop is `agent-core`'s own tests'
//! job. Each adapter crate holds one test file that implements the harness and calls these
//! checks (see `agent-provider-openai-compatible/tests/contract.rs` for the reference
//! implementation). A check that a mutation does not catch means the contract is describing
//! the wrong thing, not that the adapter is fine.

use std::sync::Mutex;
use std::time::Duration;

use agent_core::*;
use serde_json::Value;

/// One tool call in contract terms. The harness decides how `arguments` are carried on the
/// wire (OpenAI: a JSON string inside `function.arguments`; Anthropic: raw JSON blocks).
#[derive(Debug, Clone)]
pub struct FnCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The neutral reason a provider uses to finish a response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    EndTurn,
    ToolUse,
    MaxTokens,
    ContentFiltered,
    PauseTurn,
    /// A reason the adapter has never seen; it must surface as `StopReason::Other(_)` with
    /// the provider's own text, not as a silent fallback.
    Unknown,
}

/// A semantic response to serve. The harness translates each variant into its own wire
/// format; nothing here is a byte for a specific provider.
#[derive(Debug, Clone)]
pub enum Wire {
    /// One complete text response, finishing normally.
    Text(String),
    /// Text arriving in pieces. Each piece must surface as its own [`AgentEvent::ModelTextDelta`]
    /// in order, and the concatenation must equal the response's content (ADR 0003 F2: the
    /// core's interrupted-prefix feature accumulates exactly these deltas).
    TextInPieces(Vec<String>),
    /// One tool call in one response.
    ToolCall(FnCall),
    /// Several tool calls in one response, in order.
    ToolCalls(Vec<FnCall>),
    /// One tool call whose arguments arrive split across several wire chunks.
    ToolCallSplitArguments {
        id: String,
        name: String,
        pieces: Vec<String>,
    },
    /// A tool call the provider forgot to give an id to.
    ToolCallNoId { name: String, arguments: Value },
    /// A response finished with `reason`.
    Finish(Reason),
    /// The provider declines; the refusal text must surface in the content and the reason
    /// must be `Refusal`.
    Refusal(String),
    /// Usage in a tail chunk that carries no choices (OpenAI's trailing usage block).
    UsageTrailing(ModelUsage),
    /// Usage riding on several chunks; the last non-null one must win.
    UsageEveryChunk(Vec<ModelUsage>),
    /// The response carries no usage at all.
    NoUsage,
    /// The stream reports an error whose message echoes the harness's secret; the returned
    /// `ModelError` must not contain it.
    ErrorEchoesSecret,
    /// The server answers with this status and body.
    Status { code: u16, body: String },
    /// The stream is cut before the response completes. Must be an error, never a partial
    /// `ModelResponse`.
    CutMidStream(String),
    /// The server never finishes the response (used by the cancellation check).
    Hangs,
}

/// An adapter plugged into a mock server that can serve [`Wire`]s.
///
/// Implement once per provider. The harness owns: the mock, the credential it configures
/// (`secret`), and the translation from `Wire` to the provider's stream/error format.
pub trait ContractHarness<M: Model>: Sync {
    /// The credential configured into the adapter, for the leak checks.
    fn secret(&self) -> &str;

    /// Serve `wires` to successive `complete` calls and return a fresh adapter for them.
    /// The returned adapter must stay valid (the mock stays alive) for as long as it is
    /// used.
    fn script(&self, wires: &[Wire]) -> impl std::future::Future<Output = M> + Send;
}

/// A default request the checks send. One user turn, no tools.
pub fn request() -> ModelRequest {
    ModelRequest {
        system_prompt: None,
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        settings: Value::Null,
    }
}

/// A fresh context for one `complete` call.
pub fn context<'a>(events: &'a dyn EventSink) -> ModelContext<'a> {
    ModelContext {
        cancellation: CancellationToken::new(),
        events,
    }
}

/// Collects every streamed delta, in order.
#[derive(Default)]
pub struct Deltas(Mutex<Vec<String>>);

impl EventSink for Deltas {
    fn emit(&self, event: AgentEvent) {
        if let AgentEvent::ModelTextDelta { text } = event {
            self.0.lock().unwrap().push(text);
        }
    }
}

impl Deltas {
    pub fn take(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

async fn complete<M: Model>(
    model: &M,
    request: ModelRequest,
    events: &dyn EventSink,
) -> Result<ModelResponse, ModelError> {
    model.complete(request, context(events)).await
}

fn first_call(resp: &ModelResponse) -> &ToolCall {
    resp.tool_calls
        .first()
        .expect("the response must carry a tool call")
}

fn joined_text(resp: &ModelResponse) -> String {
    resp.content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---------- 1-5: text, deltas, tool calls, split arguments, id uniqueness ----------

/// 1. A plain text response: content is the text, no tool calls, `EndTurn`.
pub async fn check_plain_text<M: Model, H: ContractHarness<M>>(h: &H) {
    let model = h.script(&[Wire::Text("hello".into())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(resp.content, vec![ContentPart::text("hello")]);
    assert!(resp.tool_calls.is_empty());
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

/// 2. Streamed deltas: each piece is one `ModelTextDelta` in order, joining to the content.
pub async fn check_stream_deltas<M: Model, H: ContractHarness<M>>(h: &H) {
    let pieces = vec!["Hel".into(), "lo".into(), ", world!".into()];
    let model = h.script(&[Wire::TextInPieces(pieces.clone())]).await;
    let deltas = Deltas::default();
    let resp = complete(&model, request(), &deltas).await.unwrap();
    assert_eq!(
        deltas.take(),
        pieces,
        "every piece must surface as its own delta, in order"
    );
    assert_eq!(joined_text(&resp), "Hello, world!");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

/// 3a. One tool call: id/name/arguments copied through, `ToolUse`.
pub async fn check_single_tool_call<M: Model, H: ContractHarness<M>>(h: &H) {
    let call = FnCall {
        id: "c1".into(),
        name: "add".into(),
        arguments: serde_json::json!({ "a": 2, "b": 3 }),
    };
    let model = h.script(&[Wire::ToolCall(call)]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.tool_calls.len(), 1);
    let call = first_call(&resp);
    assert_eq!(call.id, "c1");
    assert_eq!(call.name, "add");
    assert_eq!(call.arguments, serde_json::json!({ "a": 2, "b": 3 }));
}

/// 3b. Several tool calls in one response, in order.
pub async fn check_multiple_tool_calls<M: Model, H: ContractHarness<M>>(h: &H) {
    let calls = vec![
        FnCall {
            id: "a".into(),
            name: "one".into(),
            arguments: serde_json::json!({"x": 1}),
        },
        FnCall {
            id: "b".into(),
            name: "two".into(),
            arguments: serde_json::json!({"y": 2}),
        },
        FnCall {
            id: "c".into(),
            name: "three".into(),
            arguments: serde_json::json!({"z": 3}),
        },
    ];
    let model = h.script(&[Wire::ToolCalls(calls.clone())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    let calls = resp
        .tool_calls
        .iter()
        .map(|c| (c.id.as_str(), c.name.as_str(), c.arguments.clone()))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0], ("a", "one", serde_json::json!({"x": 1})));
    assert_eq!(calls[1], ("b", "two", serde_json::json!({"y": 2})));
    assert_eq!(calls[2], ("c", "three", serde_json::json!({"z": 3})));
}

/// Arguments arriving split across chunks must be assembled into one tool call; a
/// half-cut argument must never become a call.
pub async fn check_split_arguments<M: Model, H: ContractHarness<M>>(h: &H) {
    let model = h
        .script(&[Wire::ToolCallSplitArguments {
            id: "s1".into(),
            name: "calc".into(),
            pieces: vec!["{\"a\":".into(), "2, \"b\":".into(), "3}".into()],
        }])
        .await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(
        resp.tool_calls.len(),
        1,
        "the split must rejoin into exactly one call"
    );
    let call = first_call(&resp);
    assert_eq!(call.id, "s1");
    assert_eq!(call.name, "calc");
    assert_eq!(call.arguments, serde_json::json!({"a": 2, "b": 3}));
}

/// Call ids must be unique against the conversation: when the provider repeats an id
/// already present, or gives no id at all, the adapter must generate one that does not
/// collide with the history.
pub async fn check_call_ids_unique<M: Model, H: ContractHarness<M>>(h: &H) {
    // Repeated id: the second response reuses call_0, which is already in the history we
    // send back; the adapter must not return the same id twice.
    let model = h
        .script(&[
            Wire::ToolCall(FnCall {
                id: "call_0".into(),
                name: "f".into(),
                arguments: serde_json::json!({}),
            }),
            Wire::ToolCall(FnCall {
                id: "call_0".into(),
                name: "g".into(),
                arguments: serde_json::json!({}),
            }),
        ])
        .await;
    let first = complete(&model, request(), &NoopSink).await.unwrap();
    let first_id = first_call(&first).id.clone();
    let history = vec![
        Message::user("hi"),
        Message::Assistant {
            content: Vec::new(),
            tool_calls: vec![first_call(&first).clone()],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
        Message::Tool {
            result: ToolResult::success(&first_id, Vec::new()),
            meta: Default::default(),
        },
    ];
    let second_req = ModelRequest {
        system_prompt: None,
        messages: history,
        tools: Vec::new(),
        settings: Value::Null,
    };
    let second = complete(&model, second_req.clone(), &NoopSink)
        .await
        .unwrap();
    let second_id = &second.tool_calls[0].id;
    assert!(!second_id.is_empty());
    assert_ne!(
        second_id, &first_id,
        "a repeated provider id must be renamed so the history stays unique"
    );

    // Missing id: one in an empty conversation, one after the previous one is in the
    // history; both ids must be non-empty and different.
    let model = h
        .script(&[
            Wire::ToolCallNoId {
                name: "f".into(),
                arguments: serde_json::json!({}),
            },
            Wire::ToolCallNoId {
                name: "g".into(),
                arguments: serde_json::json!({}),
            },
        ])
        .await;
    let r1 = complete(&model, request(), &NoopSink).await.unwrap();
    let id1 = first_call(&r1).id.clone();
    assert!(!id1.is_empty(), "a missing id must be made up");
    let history = vec![
        Message::user("hi"),
        Message::Assistant {
            content: Vec::new(),
            tool_calls: vec![first_call(&r1).clone()],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        },
        Message::Tool {
            result: ToolResult::success(&id1, Vec::new()),
            meta: Default::default(),
        },
    ];
    let r2_req = ModelRequest {
        system_prompt: None,
        messages: history,
        tools: Vec::new(),
        settings: Value::Null,
    };
    let r2 = complete(&model, r2_req, &NoopSink).await.unwrap();
    let id2 = &r2.tool_calls[0].id;
    assert!(!id2.is_empty());
    assert_ne!(id2, &id1, "made-up ids must not collide either");
}

// ---------- 6-10: finish reasons, usage, errors, cancellation, secrets ----------

/// Every `StopReason` variant has a fixture; an unknown reason falls into `Other` with
/// the provider's own text, not a silent fallback.
pub async fn check_finish_reasons<M: Model, H: ContractHarness<M>>(h: &H) {
    for (reason, expected) in [
        (Reason::EndTurn, StopReason::EndTurn),
        (Reason::ToolUse, StopReason::ToolUse),
        (Reason::MaxTokens, StopReason::MaxTokens),
        (Reason::ContentFiltered, StopReason::ContentFiltered),
        (Reason::PauseTurn, StopReason::PauseTurn),
    ] {
        let model = h.script(&[Wire::Finish(reason)]).await;
        let resp = complete(&model, request(), &NoopSink).await.unwrap();
        assert_eq!(resp.stop_reason, expected, "{reason:?}");
    }

    let model = h.script(&[Wire::Refusal("I can't help.".into())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::Refusal);
    assert_eq!(joined_text(&resp), "I can't help.");

    let model = h.script(&[Wire::Finish(Reason::Unknown)]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert!(
        matches!(&resp.stop_reason, StopReason::Other(raw) if !raw.is_empty()),
        "an unknown reason must surface in StopReason::Other: {:?}",
        resp.stop_reason
    );
}

/// 7a. Usage arrives: in a trailing chunk with no choices, and riding on every chunk (the
/// last non-null one wins); and not at all. `ModelUsage` is the neutral unit: the harness
/// maps it to its provider's field names.
pub async fn check_usage_shapes<M: Model, H: ContractHarness<M>>(h: &H) {
    // Trailing block.
    let usage = ModelUsage {
        input: Some(11),
        output: Some(4),
        total: Some(15),
        cache_read: Some(3),
        cache_write: None,
        details: Some(OpaqueJson::parse(r#"{"cached_tokens":3}"#).unwrap()),
    };
    let model = h.script(&[Wire::UsageTrailing(usage.clone())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(
        resp.usage,
        Some(usage),
        "the trailing usage block must be captured field-for-field"
    );

    // Every chunk carries usage; the last non-null one wins.
    let first = ModelUsage {
        input: Some(1),
        ..Default::default()
    };
    let last = ModelUsage {
        input: Some(9),
        output: Some(2),
        ..Default::default()
    };
    let model = h
        .script(&[Wire::UsageEveryChunk(vec![first, last.clone()])])
        .await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(
        resp.usage,
        Some(last),
        "the last non-null usage chunk must win"
    );

    // No usage anywhere.
    let model = h.script(&[Wire::NoUsage]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_eq!(resp.usage, None);
}

/// 7b. `total` is never computed by the adapter: input and output alone stay `total: None`.
pub async fn check_total_not_computed<M: Model, H: ContractHarness<M>>(h: &H) {
    let usage = ModelUsage {
        input: Some(7),
        output: Some(2),
        ..Default::default()
    };
    let model = h.script(&[Wire::UsageTrailing(usage.clone())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    let usage = resp.usage.expect("usage was reported");
    assert_eq!(usage.input, Some(7));
    assert_eq!(usage.output, Some(2));
    assert_eq!(
        usage.total, None,
        "total must stay None, never input + output"
    );
}

/// Server errors are `Transport`, request errors are `Protocol`, and a stream cut before
/// completion is an error, never a partial `ModelResponse`.
pub async fn check_error_classification<M: Model, H: ContractHarness<M>>(h: &H) {
    let model = h
        .script(&[Wire::Status {
            code: 500,
            body: "oops".into(),
        }])
        .await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Transport(m) if m.contains("HTTP 500") && m.contains("oops")),
        "a 500 is a transport error quoting the body: {error}"
    );

    let model = h
        .script(&[Wire::Status {
            code: 400,
            body: "bad request".into(),
        }])
        .await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Protocol(m) if m.contains("HTTP 400") && m.contains("bad request")),
        "a 400 is a protocol error quoting the body: {error}"
    );

    let model = h.script(&[Wire::CutMidStream("half an ans".into())]).await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    assert!(
        matches!(error, ModelError::Transport(_) | ModelError::Protocol(_)),
        "a cut stream is an error, never a partial response"
    );
}

/// Once `ModelContext.cancellation` fires, `complete` must return `Err` soon, never an
/// incomplete `ModelResponse`.
pub async fn check_cancellation<M: Model, H: ContractHarness<M>>(h: &H) {
    // Token already cancelled before the call: even a server that would answer instantly
    // must not turn into a response.
    let model = h.script(&[Wire::Text("hi".into())]).await;
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let cx = ModelContext {
        cancellation: cancelled,
        events: &NoopSink,
    };
    let result = model.complete(request(), cx).await;
    assert!(result.is_err(), "a cancelled request must return an error");

    // Cancellation while the request is in flight against a server that never answers.
    // The watchdog bounds the suite; synchronization is the cancellation token itself.
    let model = h.script(&[Wire::Hangs]).await;
    let token = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        let cx = ModelContext {
            cancellation: token.clone(),
            events: &NoopSink,
        };
        tokio::join!(
            async {
                tokio::task::yield_now().await;
                token.cancel();
            },
            model.complete(request(), cx),
        )
        .1
    })
    .await
    .expect("the cancelled call returns without the watchdog");
    assert!(
        matches!(&result, Err(ModelError::Transport(m)) if m.contains("cancelled")),
        "an in-flight cancellation surfaces as a transport error: {result:?}"
    );
}

/// A server that echoes the credential in an error body must end up masked in the
/// returned `ModelError`. The harness picks its own secret.
pub async fn check_secret_not_leaked<M: Model, H: ContractHarness<M>>(h: &H) {
    let secret = h.secret();
    assert!(!secret.is_empty());
    let model = h.script(&[Wire::ErrorEchoesSecret]).await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    let message = error.to_string();
    assert!(
        !message.contains(secret),
        "the credential must be masked in errors: {message}"
    );
    // The error is still an error with a real body, not an empty placeholder.
    assert!(!message.is_empty());
}
