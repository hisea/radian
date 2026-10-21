//! Adapter-neutral provider contract suite.
//!
//! Phase 0a's completion criterion asks for scripted/HTTP fixture contract tests per
//! provider. This crate pins what **any** [`Model`] implementation must do, described in
//! semantic terms (a [`Wire`]), and leaves the translation to each provider's line format to
//! the adapter itself (a [`ContractHarness`] implementation). Phase 3's Anthropic adapter
//! implements the same harness against its event stream and reuses every check.
//!
//! The checks drive [`Model::complete`] directly (not through `Agent`): the contract is
//! about the adapter, and the round trip through the turn loop is `radian-core`'s own tests'
//! job. Each adapter crate holds one test file that implements the harness and calls these
//! checks (see `agent-provider-openai-compatible/tests/contract.rs` for the reference
//! implementation). A check that a mutation does not catch means the contract is describing
//! the wrong thing, not that the adapter is fine.

use std::sync::Mutex;
use std::time::Duration;

use radian_core::*;
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

/// One field of [`ModelUsage`]. Used to declare which fields a provider does not report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageField {
    Input,
    Output,
    Total,
    CacheRead,
    CacheWrite,
    Details,
}

impl UsageField {
    pub fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Output => "output",
            Self::Total => "total",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::Details => "details",
        }
    }

    /// Whether `usage` carries a value for this field.
    pub fn is_reported(self, usage: &ModelUsage) -> bool {
        match self {
            Self::Input => usage.input.is_some(),
            Self::Output => usage.output.is_some(),
            Self::Total => usage.total.is_some(),
            Self::CacheRead => usage.cache_read.is_some(),
            Self::CacheWrite => usage.cache_write.is_some(),
            Self::Details => usage.details.is_some(),
        }
    }

    /// Clears this field. A harness uses it to model "the provider does not report this";
    /// [`expected_usage`] uses it to build what the adapter is required to return.
    pub fn clear(self, usage: &mut ModelUsage) {
        match self {
            Self::Input => usage.input = None,
            Self::Output => usage.output = None,
            Self::Total => usage.total = None,
            Self::CacheRead => usage.cache_read = None,
            Self::CacheWrite => usage.cache_write = None,
            Self::Details => usage.details = None,
        }
    }
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

    /// The finish reasons this provider has **no wire form for**. A reason listed here is
    /// skipped by [`check_finish_reasons`]; a reason that is *not* listed must be satisfied.
    ///
    /// There is deliberately no silent skip: an adapter must declare the gap here, with the
    /// provider's documentation as the reason, so the suite never pressures an adapter into
    /// inventing a value its provider does not send. The default is "supports every reason".
    fn unsupported_reasons(&self) -> &'static [Reason] {
        &[]
    }

    /// The [`ModelUsage`] fields this provider does **not** report. A declared field is not
    /// skipped: the contract requires it to be `None` in every response (see
    /// [`expected_usage`]). This hatch makes the check **stricter**, not looser — a provider
    /// that does not report a number must never have one invented for it (ADR 0007 J2). The
    /// default is "reports every field".
    fn unreported_usage_fields(&self) -> &'static [UsageField] {
        &[]
    }

    /// Whether this provider's reported input count **contains** the cache hit: `true` for the
    /// OpenAI shape (`prompt_tokens` contains `prompt_tokens_details.cached_tokens`), `false`
    /// for the Anthropic shape (`input_tokens` is the uncached part, a sibling of
    /// `cache_read_input_tokens`). [`check_input_cache_semantics`] requires the adapter to
    /// mark every usage it reports with this value (ADR 0007).
    ///
    /// **There is no "unset"**: a provider whose usage has no cache concept at all reports
    /// `false`, which is the truthful answer for its shape (everything it counts is
    /// uncached). The default here is `false`, so a new harness states the OpenAI shape
    /// explicitly rather than inheriting it by accident.
    fn input_includes_cache(&self) -> bool {
        false
    }

    /// The provider's **real** "the input is over the context window" error, as a
    /// [`Wire::Status`], or `None` when this provider has no recognizable one.
    ///
    /// `None` is a declaration, not a skip: [`check_context_overflow`] then still asserts that
    /// a plain 400, a 429 "too many tokens" and a 500 naming `context_length_exceeded` are
    /// **not** classified as overflows. A harness that declares `Some` must have that exact
    /// wire become [`ModelError::ContextOverflow`]. The default is `None`.
    fn context_overflow(&self) -> Option<Wire> {
        None
    }
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
            reasoning: Vec::new(),
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
            reasoning: Vec::new(),
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

/// Every `Reason` the suite can script, in check order.
const ALL_REASONS: [Reason; 5] = [
    Reason::EndTurn,
    Reason::ToolUse,
    Reason::MaxTokens,
    Reason::ContentFiltered,
    Reason::PauseTurn,
];

/// The neutral stop reason a `Reason` must surface as.
fn expected_stop_reason(reason: Reason) -> StopReason {
    match reason {
        Reason::EndTurn => StopReason::EndTurn,
        Reason::ToolUse => StopReason::ToolUse,
        Reason::MaxTokens => StopReason::MaxTokens,
        Reason::ContentFiltered => StopReason::ContentFiltered,
        Reason::PauseTurn => StopReason::PauseTurn,
        // `Unknown` is checked separately and must land in `Other`.
        Reason::Unknown => StopReason::Other(String::new()),
    }
}

/// The reasons a check must exercise for this harness: every reason except those the harness
/// explicitly declares unsupported. Exposed so the escape hatch itself is testable.
pub fn reasons_to_check<M: Model, H: ContractHarness<M>>(h: &H) -> Vec<Reason> {
    let unsupported = h.unsupported_reasons();
    ALL_REASONS
        .iter()
        .copied()
        .filter(|reason| !unsupported.contains(reason))
        .collect()
}

/// Every `StopReason` variant has a fixture; an unknown reason falls into `Other` with
/// the provider's own text, not a silent fallback. Reasons the harness explicitly declares
/// unsupported (see [`ContractHarness::unsupported_reasons`]) are skipped, and only an
/// explicit declaration skips them.
pub async fn check_finish_reasons<M: Model, H: ContractHarness<M>>(h: &H) {
    for reason in reasons_to_check(h) {
        let model = h.script(&[Wire::Finish(reason)]).await;
        let resp = complete(&model, request(), &NoopSink).await.unwrap();
        assert_eq!(resp.stop_reason, expected_stop_reason(reason), "{reason:?}");
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

/// The usage the adapter must return for `scripted`: the scripted values, except that every
/// field the harness declares unreported must be `None`. This is deliberately **not** a skip:
/// a declared field is still asserted, just asserted absent, so an adapter that invents a
/// value for a field its provider never reports fails the check (ADR 0007 J2).
///
/// `input_includes_cache` is not a wire field: the harness's [`ContractHarness::script`]
/// cannot carry it, and the adapter derives it from its own provider contract. So the
/// expectation takes the harness's declaration, and the check still asserts the adapter's
/// value (ADR 0007).
pub fn expected_usage<M: Model, H: ContractHarness<M>>(h: &H, scripted: &ModelUsage) -> ModelUsage {
    let mut expected = scripted.clone();
    for field in h.unreported_usage_fields() {
        field.clear(&mut expected);
    }
    expected.input_includes_cache = Some(h.input_includes_cache());
    expected
}

/// Asserts that every field the harness declared unreported is `None` in `usage`, with a
/// message that names the field. A field the provider does not report must never carry a
/// value the adapter made up.
fn assert_unreported_usage_absent<M: Model, H: ContractHarness<M>>(
    h: &H,
    usage: Option<&ModelUsage>,
) {
    for field in h.unreported_usage_fields() {
        let reported = usage.is_some_and(|usage| field.is_reported(usage));
        assert!(
            !reported,
            "the adapter reported `{}` although the harness declares the provider does not \
             report it; a provider that does not report a number must never have one invented \
             for it (ADR 0007 J2)",
            field.name()
        );
    }
}

/// 7a. Usage arrives: in a trailing chunk with no choices, and riding on every chunk (the
/// last non-null one wins); and not at all. `ModelUsage` is the neutral unit: the harness
/// maps it to its provider's field names. Fields the harness declares unreported must be
/// `None` in the result — asserted, never skipped.
/// Compares two `ModelUsage` values, but `details` **by JSON content, not by bytes**.
///
/// `OpaqueJson` equality is byte-exact, and an adapter that parses a provider chunk into
/// `serde_json::Value` loses the original key order (`Value`'s map is a `BTreeMap`), which
/// ADR 0007 J3 records as accepted. Comparing bytes here would force every expectation to be
/// written in whatever order one adapter's parser happens to emit — encoding an
/// implementation detail into a provider-neutral contract. The sub-objects' content and
/// nesting are what the contract is about.
fn assert_usage_eq(actual: Option<&ModelUsage>, expected: &ModelUsage, what: &str) {
    let actual = actual.unwrap_or_else(|| panic!("{what}: no usage at all"));
    let json = |usage: &ModelUsage| -> Option<Value> {
        usage
            .details
            .as_ref()
            .map(|d| serde_json::from_str(d.get()).expect("details is JSON"))
    };
    assert_eq!(json(actual), json(expected), "{what}: details content");
    let strip = |usage: &ModelUsage| ModelUsage {
        details: None,
        ..usage.clone()
    };
    assert_eq!(strip(actual), strip(expected), "{what}");
}

pub async fn check_usage_shapes<M: Model, H: ContractHarness<M>>(h: &H) {
    // The hatch must not be usable to opt out of usage altogether: an adapter that declares
    // every field unreported would turn this check into a no-op and pass without parsing
    // usage at all. A provider that genuinely reports nothing is a real provider behavior,
    // not something a harness may declare its way into.
    let all = [
        UsageField::Input,
        UsageField::Output,
        UsageField::Total,
        UsageField::CacheRead,
        UsageField::CacheWrite,
        UsageField::Details,
    ];
    let unreported = h.unreported_usage_fields();
    assert!(
        all.iter().any(|field| !unreported.contains(field)),
        "a harness may not declare every usage field unreported: that would make this check \
         vacuous. If the provider really reports no usage at all, that needs a decision, not \
         a declaration."
    );

    // Trailing block.
    let usage = ModelUsage {
        input: Some(11),
        output: Some(4),
        total: Some(15),
        cache_read: Some(3),
        cache_write: None,
        input_includes_cache: Some(false),
        details: Some(
            OpaqueJson::parse(
                r#"{"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":30}}"#,
            )
            .unwrap(),
        ),
    };
    let model = h.script(&[Wire::UsageTrailing(usage.clone())]).await;
    let resp = complete(&model, request(), &NoopSink).await.unwrap();
    assert_unreported_usage_absent(h, resp.usage.as_ref());
    assert_usage_eq(
        resp.usage.as_ref(),
        &expected_usage(h, &usage),
        "the trailing usage block must be captured field-for-field, except that a provider \
         which does not report a field must leave it None",
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
    assert_unreported_usage_absent(h, resp.usage.as_ref());
    assert_usage_eq(
        resp.usage.as_ref(),
        &expected_usage(h, &last),
        "the last non-null usage chunk must win",
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

/// 7c. The adapter **declares** whether `input` contains the cache hit (ADR 0007),
/// and it declares the same thing for every response it produces.
///
/// The two spellings are both legitimate — OpenAI's `prompt_tokens` contains
/// `prompt_tokens_details.cached_tokens` (`Some(true)`), Anthropic's `input_tokens` is a
/// sibling of the cache fields (`Some(false)`) — so the contract does not pick one. What it
/// refuses is silence: a `None` here would make every consumer guess, and guessing is exactly
/// what priced the cached tokens twice. A harness states it with
/// [`ContractHarness::input_includes_cache`]; a provider whose usage has no cache concept at
/// all declares `Some(false)` (the `input` it reports is all uncached by construction).
pub async fn check_input_cache_semantics<M: Model, H: ContractHarness<M>>(h: &H) {
    let declared = h.input_includes_cache();
    let usage = ModelUsage {
        input: Some(11),
        output: Some(4),
        total: Some(15),
        cache_read: Some(3),
        cache_write: None,
        details: None,
        ..Default::default()
    };
    for wires in [
        vec![Wire::UsageTrailing(usage.clone())],
        vec![Wire::UsageEveryChunk(vec![
            ModelUsage {
                input: Some(1),
                ..Default::default()
            },
            usage.clone(),
        ])],
        vec![Wire::NoUsage],
    ] {
        let model = h.script(&wires).await;
        let resp = complete(&model, request(), &NoopSink).await.unwrap();
        let Some(usage) = resp.usage else {
            continue; // `Wire::NoUsage`: nothing to declare anything about.
        };
        assert_eq!(
            usage.input_includes_cache,
            Some(declared),
            "the adapter must mark every reported usage with the declaration it made \
             (`input_includes_cache = {declared}`); `None` leaves every consumer guessing \
             (ADR 0007)"
        );
    }
}

/// Server errors are `Unavailable` (ADR 0017: the transient kind the core retries),
/// request errors are `Protocol`, and a stream cut before completion is an error, never a
/// partial `ModelResponse`.
pub async fn check_error_classification<M: Model, H: ContractHarness<M>>(h: &H) {
    let model = h
        .script(&[Wire::Status {
            code: 500,
            body: "oops".into(),
        }])
        .await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    assert!(
        matches!(&error, ModelError::Unavailable { message, .. }
            if message.contains("HTTP 500") && message.contains("oops")),
        "a 500 is an unavailable error quoting the body: {error}"
    );
    assert!(
        error.is_transient(),
        "a 500 must be transient so the core retries it: {error}"
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
    assert!(
        !error.is_transient(),
        "a 400 must never be retried: {error}"
    );

    let model = h.script(&[Wire::CutMidStream("half an ans".into())]).await;
    let error = complete(&model, request(), &NoopSink).await.unwrap_err();
    assert!(
        matches!(error, ModelError::Transport(_) | ModelError::Protocol(_)),
        "a cut stream is an error, never a partial response"
    );
}

/// ADR 0018's fallback: an adapter returns [`ModelError::ContextOverflow`] **only** for a
/// provider error it can recognize as an over-window input. Two parts:
///
/// 1. **Every** provider, whatever it declares: a plain `400`, a `429` that merely says "too
///    many tokens", and a `500` that names `context_length_exceeded` are ordinary errors,
///    never an overflow. This part runs unconditionally, so a harness cannot opt out of it.
/// 2. A harness that declares [`ContractHarness::context_overflow`] must have that exact wire
///    classified as `ContextOverflow`, quoting the provider's body and leaking no secret.
pub async fn check_context_overflow<M: Model, H: ContractHarness<M>>(h: &H) {
    for (code, body) in [
        (400u16, "bad request"),
        // The generic phrase must never be an overflow, at any status.
        (400, "too many tokens"),
        (429, "too many tokens"),
        (500, "context_length_exceeded"),
    ] {
        let model = h
            .script(&[Wire::Status {
                code,
                body: body.into(),
            }])
            .await;
        let error = complete(&model, request(), &NoopSink).await.unwrap_err();
        assert!(
            !matches!(error, ModelError::ContextOverflow(_)),
            "HTTP {code} {body:?} must not be an overflow: {error}"
        );
    }

    match h.context_overflow() {
        Some(Wire::Status { code, body }) => {
            let model = h
                .script(&[Wire::Status {
                    code,
                    body: body.clone(),
                }])
                .await;
            let error = complete(&model, request(), &NoopSink).await.unwrap_err();
            let ModelError::ContextOverflow(text) = &error else {
                panic!("the declared overflow wire must be ContextOverflow: {error}");
            };
            assert!(
                text.contains(&body),
                "the overflow error must quote the provider body: {text}"
            );
            assert!(
                !text.contains(h.secret()),
                "the overflow error must not leak the secret: {text}"
            );
        }
        Some(other) => panic!("context_overflow must be a Wire::Status, got {other:?}"),
        None => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake provider. Its only flaws are the ones the tests inject: a wrong
    /// `ContentFiltered` mapping, the `drop_usage` fields it fails to report, and an optional
    /// error (`error`) so the overflow hatch can be exercised.
    struct FakeModel {
        stop_reason: StopReason,
        text: String,
        usage: Option<ModelUsage>,
        error: Option<ModelError>,
    }

    /// `ModelError` is not `Clone`; the fake needs to hand out the same error on every call.
    fn clone_error(error: &ModelError) -> ModelError {
        match error {
            ModelError::Transport(text) => ModelError::Transport(text.clone()),
            ModelError::Protocol(text) => ModelError::Protocol(text.clone()),
            ModelError::Unavailable {
                message,
                retry_after,
            } => ModelError::Unavailable {
                message: message.clone(),
                retry_after: *retry_after,
            },
            ModelError::ContextOverflow(text) => ModelError::ContextOverflow(text.clone()),
            ModelError::EmptyResponse => ModelError::EmptyResponse,
        }
    }

    impl Model for FakeModel {
        async fn complete(
            &self,
            _request: ModelRequest,
            _cx: ModelContext<'_>,
        ) -> Result<ModelResponse, ModelError> {
            if let Some(error) = &self.error {
                return Err(clone_error(error));
            }
            Ok(ModelResponse {
                limits: None,
                reasoning: Vec::new(),
                content: if self.text.is_empty() {
                    Vec::new()
                } else {
                    vec![ContentPart::text(self.text.clone())]
                },
                tool_calls: Vec::new(),
                provider_payload: None,
                stop_reason: self.stop_reason.clone(),
                usage: self.usage.clone(),
            })
        }
    }

    struct FakeHarness {
        unsupported: &'static [Reason],
        /// What the harness tells the contract the provider does not report.
        unreported_usage: &'static [UsageField],
        /// What the fake adapter actually leaves out when it builds a response.
        drop_usage: &'static [UsageField],
        /// What the harness declares about the input's cache semantics.
        input_includes_cache: bool,
        /// Whether the fake adapter forgets to mark the usage it reports (a silent adapter).
        forget_input_cache: bool,
        /// The provider's declared overflow wire (`None` = "no recognizable one").
        overflow: Option<Wire>,
        /// Whether the fake adapter wrongly classifies **every** 400/413 as an overflow.
        misclassify_all_4xx: bool,
        /// Whether the fake adapter fails to recognize the wire the harness declared.
        forget_overflow: bool,
    }

    impl FakeHarness {
        fn new() -> Self {
            Self {
                unsupported: &[],
                unreported_usage: &[],
                drop_usage: &[],
                input_includes_cache: false,
                forget_input_cache: false,
                overflow: None,
                misclassify_all_4xx: false,
                forget_overflow: false,
            }
        }
    }

    impl ContractHarness<FakeModel> for FakeHarness {
        fn secret(&self) -> &str {
            "fake-secret"
        }

        async fn script(&self, wires: &[Wire]) -> FakeModel {
            let mut usage = match wires.first() {
                Some(Wire::UsageTrailing(usage)) => Some(usage.clone()),
                Some(Wire::UsageEveryChunk(usages)) => usages.last().cloned(),
                _ => None,
            };
            if let Some(usage) = usage.as_mut() {
                for field in self.drop_usage {
                    field.clear(usage);
                }
                if !self.forget_input_cache {
                    usage.input_includes_cache = Some(self.input_includes_cache);
                }
            }
            let (stop_reason, text) = match wires.first() {
                // Deliberately wrong: `ContentFiltered` surfaces as `EndTurn`.
                Some(Wire::Finish(Reason::ContentFiltered)) => (StopReason::EndTurn, "x".into()),
                Some(Wire::Finish(Reason::Unknown)) => {
                    (StopReason::Other("unknown".into()), "x".into())
                }
                Some(Wire::Refusal(text)) => (StopReason::Refusal, text.clone()),
                Some(Wire::Finish(reason)) => (expected_stop_reason(*reason), "x".into()),
                _ => (StopReason::EndTurn, "x".into()),
            };
            // A `Wire::Status` is an error, not a response; the declared overflow wire is the
            // one the fake is willing to recognize.
            let error = match wires.first() {
                Some(Wire::Status { code, body }) => {
                    let declared = !self.forget_overflow
                        && matches!(
                            &self.overflow,
                            Some(Wire::Status { code: c, body: b }) if c == code && b == body
                        );
                    if declared || (self.misclassify_all_4xx && matches!(code, 400 | 413)) {
                        Some(ModelError::ContextOverflow(format!("HTTP {code}: {body}")))
                    } else if *code >= 500 || matches!(code, 408 | 429) {
                        Some(ModelError::Unavailable {
                            message: format!("HTTP {code}: {body}"),
                            retry_after: None,
                        })
                    } else {
                        Some(ModelError::Protocol(format!("HTTP {code}: {body}")))
                    }
                }
                _ => None,
            };
            FakeModel {
                stop_reason,
                text,
                usage,
                error,
            }
        }

        fn unsupported_reasons(&self) -> &'static [Reason] {
            self.unsupported
        }

        fn unreported_usage_fields(&self) -> &'static [UsageField] {
            self.unreported_usage
        }

        fn input_includes_cache(&self) -> bool {
            self.input_includes_cache
        }

        fn context_overflow(&self) -> Option<Wire> {
            self.overflow.clone()
        }
    }

    // ---------- the reason hatch ----------

    #[test]
    fn the_default_declarations_support_everything() {
        let harness = FakeHarness::new();
        assert_eq!(harness.unsupported_reasons(), &[]);
        assert_eq!(harness.unreported_usage_fields(), &[]);
        assert_eq!(reasons_to_check(&harness), ALL_REASONS.to_vec());
    }

    #[test]
    fn only_declared_reasons_are_dropped_from_the_check() {
        let harness = FakeHarness {
            unsupported: &[Reason::ContentFiltered],
            ..FakeHarness::new()
        };
        let checked = reasons_to_check(&harness);
        assert!(!checked.contains(&Reason::ContentFiltered));
        for reason in [
            Reason::EndTurn,
            Reason::ToolUse,
            Reason::MaxTokens,
            Reason::PauseTurn,
        ] {
            assert!(
                checked.contains(&reason),
                "{reason:?} must still be checked"
            );
        }
    }

    #[tokio::test]
    async fn a_declared_reason_is_skipped() {
        // The fake maps ContentFiltered wrongly; declaring it unsupported is what lets the
        // check pass. Refusal and Unknown are always checked and the fake satisfies them.
        let harness = FakeHarness {
            unsupported: &[Reason::ContentFiltered],
            ..FakeHarness::new()
        };
        check_finish_reasons(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "ContentFiltered")]
    async fn an_undeclared_reason_must_be_satisfied() {
        let harness = FakeHarness::new();
        check_finish_reasons(&harness).await;
    }

    // ---------- the usage hatch ----------

    #[test]
    fn expected_usage_clears_only_declared_fields() {
        let scripted = ModelUsage {
            input: Some(1),
            output: Some(2),
            total: Some(3),
            cache_read: Some(4),
            cache_write: Some(5),
            input_includes_cache: Some(true),
            details: Some(OpaqueJson::parse(r#"{"x":1}"#).unwrap()),
        };
        let harness = FakeHarness {
            unreported_usage: &[UsageField::Total, UsageField::Details],
            ..FakeHarness::new()
        };
        let expected = expected_usage(&harness, &scripted);
        assert_eq!(expected.input, Some(1));
        assert_eq!(expected.output, Some(2));
        assert_eq!(expected.total, None, "a declared field is cleared");
        assert_eq!(expected.cache_read, Some(4));
        assert_eq!(expected.cache_write, Some(5));
        assert_eq!(expected.details, None, "a declared field is cleared");
        // The mark is the harness's declaration, not the scripted value: the fake harness
        // leaves it `false` and the expectation says so.
        assert_eq!(expected.input_includes_cache, Some(false));
    }

    #[tokio::test]
    async fn a_declared_unreported_field_must_be_absent() {
        // The honest fake adapter drops `total`, and the harness declares it unreported:
        // the check passes because the contract requires exactly that.
        let harness = FakeHarness {
            unreported_usage: &[UsageField::Total],
            drop_usage: &[UsageField::Total],
            ..FakeHarness::new()
        };
        check_usage_shapes(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "reported `total`")]
    async fn a_declared_field_that_is_filled_fails() {
        // The harness declares `total` unreported but the adapter invents one anyway: the
        // hatch is an assertion of absence, not a skip, so this must fail.
        let harness = FakeHarness {
            unreported_usage: &[UsageField::Total],
            drop_usage: &[],
            ..FakeHarness::new()
        };
        check_usage_shapes(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "trailing usage block")]
    async fn an_undeclared_field_must_be_reported() {
        // The adapter silently drops `total` without declaring it: the contract expects the
        // scripted value and must notice the loss.
        let harness = FakeHarness {
            unreported_usage: &[],
            drop_usage: &[UsageField::Total],
            ..FakeHarness::new()
        };
        check_usage_shapes(&harness).await;
    }

    // ---------- `input_includes_cache` ----------

    #[test]
    fn the_default_declaration_is_input_without_the_cache_and_expected_usage_carries_it() {
        // No "unset": an unstated harness is `false`, and the expectation carries it, so the
        // check asserts a value rather than tolerating silence (ADR 0007).
        let harness = FakeHarness::new();
        assert!(!harness.input_includes_cache());
        assert_eq!(
            expected_usage(&harness, &ModelUsage::default()).input_includes_cache,
            Some(false)
        );
        let openai = FakeHarness {
            input_includes_cache: true,
            ..FakeHarness::new()
        };
        assert_eq!(
            expected_usage(&openai, &ModelUsage::default()).input_includes_cache,
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_marked_usage_passes_and_the_openai_shape_declares_true() {
        for declared in [false, true] {
            let harness = FakeHarness {
                input_includes_cache: declared,
                ..FakeHarness::new()
            };
            check_input_cache_semantics(&harness).await;
        }
    }

    #[tokio::test]
    #[should_panic(expected = "leaves every consumer guessing")]
    async fn an_unmarked_usage_fails_the_cache_semantics_check() {
        // The silent adapter: it parses the numbers but never says whether the cache hit is
        // inside `input`. That is the state ADR 0007 exists to end.
        let harness = FakeHarness {
            forget_input_cache: true,
            ..FakeHarness::new()
        };
        check_input_cache_semantics(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "leaves every consumer guessing")]
    async fn a_marked_usage_that_contradicts_the_declaration_fails() {
        // The adapter always marks `true` while the harness declares `false`: the two must
        // agree, or a consumer is guessing which one to believe.
        let harness = FakeHarness {
            input_includes_cache: false,
            ..FakeHarness::new()
        };
        let wrong = FakeHarness {
            input_includes_cache: true,
            ..FakeHarness::new()
        };
        // Sanity: the honest pair passes, so the failure below is the contradiction.
        check_input_cache_semantics(&harness).await;
        struct AlwaysTrue(FakeHarness);
        impl ContractHarness<FakeModel> for AlwaysTrue {
            fn secret(&self) -> &str {
                self.0.secret()
            }
            async fn script(&self, wires: &[Wire]) -> FakeModel {
                let model = self.0.script(wires).await;
                FakeModel {
                    usage: model.usage.map(|mut usage| {
                        usage.input_includes_cache = Some(true);
                        usage
                    }),
                    ..model
                }
            }
            fn input_includes_cache(&self) -> bool {
                false
            }
        }
        check_input_cache_semantics(&AlwaysTrue(wrong)).await;
    }

    // ---------- the context-overflow hatch ----------

    fn overflow_wire() -> Wire {
        Wire::Status {
            code: 400,
            body: r#"{"error":{"message":"maximum context length is 100 tokens","code":"context_length_exceeded"}}"#
                .into(),
        }
    }

    #[tokio::test]
    async fn a_declared_context_overflow_is_classified() {
        // The fake recognizes exactly the declared wire; the negatives are ordinary errors.
        let harness = FakeHarness {
            overflow: Some(overflow_wire()),
            ..FakeHarness::new()
        };
        check_context_overflow(&harness).await;
    }

    #[tokio::test]
    async fn no_declaration_skips_only_the_positive_check() {
        // A provider with no recognizable overflow error passes: the negatives still ran.
        let harness = FakeHarness::new();
        assert!(harness.context_overflow().is_none());
        check_context_overflow(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "must not be an overflow")]
    async fn a_plain_400_is_never_an_overflow_even_without_a_declaration() {
        // The fake classifies every 400/413 as an overflow. The negative checks run for every
        // harness, declared or not, so this must fail.
        let harness = FakeHarness {
            misclassify_all_4xx: true,
            ..FakeHarness::new()
        };
        check_context_overflow(&harness).await;
    }

    #[tokio::test]
    #[should_panic(expected = "must be ContextOverflow")]
    async fn a_declared_overflow_that_is_not_classified_fails() {
        // The harness declares a wire the fake does **not** recognize (a different body), so
        // the positive check must fail rather than pass vacuously.
        let harness = FakeHarness {
            overflow: Some(Wire::Status {
                code: 400,
                body: "some other 400".into(),
            }),
            forget_overflow: true,
            ..FakeHarness::new()
        };
        check_context_overflow(&harness).await;
    }
}
