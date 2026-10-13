//! Automatic compaction's pure planning half (ADR 0018): `keep_target`, `units`, `plan`,
//! `summary_request`, `check_summary` and `compacted_history`.

use agent_core::{
    ContentPart, InputSource, Message, MessageMeta, PlanError, SummaryError, ToolCall, ToolResult,
    check_summary, compacted_history, keep_target, message_tokens, plan, summary_request, units,
};
use serde_json::{Value, json};

// ---------- builders ----------

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "echo".into(),
        arguments: json!({}),
    }
}

fn assistant_calls(ids: &[&str]) -> Message {
    Message::Assistant {
        content: Vec::new(),
        reasoning: Vec::new(),
        tool_calls: ids.iter().map(|id| call(id)).collect(),
        provider_payload: None,
        interrupted: false,
        meta: MessageMeta::default(),
    }
}

fn assistant_with_reasoning(text: &str, reasoning: &str) -> Message {
    Message::Assistant {
        content: vec![ContentPart::text(text)],
        reasoning: vec![ContentPart::text(reasoning)],
        tool_calls: Vec::new(),
        provider_payload: None,
        interrupted: false,
        meta: MessageMeta::default(),
    }
}

fn tool(id: &str) -> Message {
    Message::Tool {
        result: ToolResult::success(id, vec![ContentPart::text("ok")]),
        meta: MessageMeta::default(),
    }
}

fn tool_with_text(id: &str, text: &str) -> Message {
    Message::Tool {
        result: ToolResult::success(id, vec![ContentPart::text(text)]),
        meta: MessageMeta::default(),
    }
}

/// The ADR 0002/0003 repaired result: an `interrupted` tool result the core appends for a call
/// that never ran.
fn tool_interrupted(id: &str) -> Message {
    Message::Tool {
        result: ToolResult::error(id, "interrupted", "The run was interrupted."),
        meta: MessageMeta::default(),
    }
}

// ---------- keep_target ----------

#[test]
fn keep_target_clamps_the_quarter_of_the_budget() {
    // A quarter of 4000 is 1000, below the floor: the floor wins.
    assert_eq!(keep_target(4_000), 2_048);
    // A quarter of 40000 is 10000, inside the range.
    assert_eq!(keep_target(40_000), 10_000);
    // A quarter of 1000000 is 250000, above the ceiling: the ceiling wins.
    assert_eq!(keep_target(1_000_000), 20_000);
    // The boundaries themselves are inclusive.
    assert_eq!(keep_target(8_192), 2_048);
    assert_eq!(keep_target(80_000), 20_000);
}

// ---------- units ----------

#[test]
fn an_assistant_and_all_its_results_are_one_unit() {
    let history = vec![
        Message::user("go"),
        // Two parallel calls: both results belong to this assistant message.
        assistant_calls(&["a", "b"]),
        tool("a"),
        tool("b"),
        Message::assistant("done"),
    ];
    let ranges = units(&history).unwrap();
    assert_eq!(ranges, vec![0..1, 1..4, 4..5]);
    // Contiguous and covering the whole history.
    assert_eq!(ranges[0].start, 0);
    assert_eq!(ranges.last().unwrap().end, history.len());
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].end, pair[1].start);
    }
}

#[test]
fn an_interrupted_result_stays_with_its_call() {
    let history = vec![
        assistant_calls(&["a"]),
        tool_interrupted("a"),
        Message::user("again"),
    ];
    assert_eq!(units(&history).unwrap(), vec![0..2, 2..3]);
}

#[test]
fn a_tool_chain_can_be_split_at_one_of_its_assistant_messages() {
    // Two assistant messages each with one call: each assistant + its result is its own unit,
    // so a cut between them is legal (pi's split turn).
    let history = vec![
        assistant_calls(&["a"]),
        tool("a"),
        assistant_calls(&["b"]),
        tool("b"),
    ];
    assert_eq!(units(&history).unwrap(), vec![0..2, 2..4]);
}

#[test]
fn units_rejects_an_invalid_history() {
    // A call with no result.
    assert!(units(&[assistant_calls(&["a"])]).is_err());
    // A result with no call.
    assert!(units(&[tool("a")]).is_err());
    // A result for an unknown call after a user message.
    assert!(units(&[Message::user("go"), tool("x")]).is_err());
    // A duplicate call id.
    assert!(units(&[assistant_calls(&["a", "a"]), tool("a"), tool("a")]).is_err());
}

// ---------- plan ----------

#[test]
fn plan_cuts_only_at_a_unit_boundary() {
    // A per-message cut would stop inside the second assistant's parallel results.
    let history = vec![
        Message::user("u"),
        assistant_calls(&["a", "b"]),
        tool("a"),
        tool("b"),
        assistant_calls(&["c", "d"]),
        tool("c"),
        tool("d"),
        Message::assistant("done"),
    ];
    let ranges = units(&history).unwrap();
    assert_eq!(ranges, vec![0..1, 1..4, 4..7, 7..8]);

    // K = 2048; each message is "1000 tokens". Only the last unit (1000) fits.
    let small = plan(&history, 8_192, &|_| 1_000).unwrap();
    assert_eq!(small.first_kept, 7, "the cut must be a unit boundary");
    assert!(
        ranges.iter().any(|u| u.start == small.first_kept),
        "first_kept is not a unit start: {small:?}"
    );
    assert_eq!(small.summarized, 0..7);
    assert_eq!(small.kept_tokens, 1_000);
    assert!(small.kept_tokens <= 2_048);

    // K = 5000: the last two units (1000 + 3000) fit, so the cut moves earlier, still to a
    // boundary (before the parallel call at index 4).
    let large = plan(&history, 20_000, &|_| 1_000).unwrap();
    assert_eq!(large.first_kept, 4);
    assert_eq!(large.kept_tokens, 4_000);
    assert!(large.kept_tokens <= 5_000);
}

#[test]
fn an_oversized_last_unit_is_still_kept_whole() {
    let history = vec![Message::user("big"), assistant_calls(&["a"]), tool("a")];
    // The last unit is 2 messages x 5000 = 10000, over K = 2048: it is still kept in full.
    let plan = plan(&history, 8_192, &|_| 5_000).unwrap();
    assert_eq!(plan.first_kept, 1);
    assert_eq!(plan.summarized, 0..1);
    assert_eq!(plan.kept_tokens, 10_000);
    assert_eq!(plan.summarized_tokens, 5_000);
}

#[test]
fn a_history_that_fits_has_nothing_to_compact() {
    let history = vec![Message::user("a"), Message::assistant("b")];
    assert!(matches!(
        plan(&history, 100_000, &|_| 10),
        Err(PlanError::NothingToCompact)
    ));
    // An empty history has nothing to compact either.
    assert!(matches!(
        plan(&[], 100_000, &|_| 10),
        Err(PlanError::NothingToCompact)
    ));
    // An invalid history is an error, not a plan.
    assert!(matches!(
        plan(&[assistant_calls(&["a"])], 100_000, &|_| 10),
        Err(PlanError::InvalidHistory(_))
    ));
}

// ---------- summary_request ----------

#[test]
fn the_summary_request_serializes_text_and_truncates_tool_results() {
    let long = "x".repeat(2_500);
    let history = vec![
        Message::user("hello"),
        assistant_calls(&["a"]),
        tool_with_text("a", &long),
        assistant_with_reasoning("answer", "SECRET-REASONING"),
    ];
    let request = summary_request(Some("PREVIOUS"), &history, Some("focus on the API"));

    assert!(request.tools.is_empty(), "the summarizer gets no tools");
    assert_eq!(request.settings, Value::Null);
    let system = request.system_prompt.as_deref().unwrap();
    for section in [
        "## Goal",
        "## Constraints",
        "## Progress",
        "## Key decisions",
        "## Next steps",
        "## Critical context",
        "## Files read and modified",
    ] {
        assert!(
            system.contains(section),
            "the prompt lacks {section}: {system}"
        );
    }

    let body = request.messages[0].text();
    assert!(body.contains("hello"), "{body}");
    assert!(body.contains("PREVIOUS"), "{body}");
    assert!(body.contains("focus on the API"), "{body}");
    // The tool result is cut to 2000 characters and the cut is stated.
    assert!(body.contains("truncated 500 characters"), "{body}");
    assert_eq!(body.matches('x').count(), 2_000, "{body}");
    // An assistant's reasoning is provider-invisible and must not reach the summarizer.
    assert!(!body.contains("SECRET-REASONING"), "{body}");
}

#[test]
fn the_summary_request_truncates_tool_call_arguments_too() {
    // A write-a-file call carries the whole file in its arguments.
    let history = vec![
        Message::Assistant {
            content: Vec::new(),
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "w".into(),
                name: "write".into(),
                arguments: json!({ "content": "y".repeat(5_000) }),
            }],
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        },
        tool_with_text("w", "ok"),
    ];
    let body = summary_request(None, &history, None).messages[0].text();
    assert!(body.contains("called write w with"), "{body}");
    assert!(body.contains("[truncated"), "{body}");
    assert!(body.matches('y').count() < 2_000, "{body}");
}

#[test]
fn truncating_a_tool_result_never_splits_a_character() {
    // 2500 Han characters: a byte-based cut at 2000 would split one.
    let long = "中".repeat(2_500);
    let history = vec![assistant_calls(&["a"]), tool_with_text("a", &long)];
    let request = summary_request(None, &history, None);
    let body = request.messages[0].text();
    assert!(body.contains("truncated 500 characters"), "{body}");
    assert_eq!(body.matches('中').count(), 2_000, "{body}");
}

#[test]
fn a_summary_request_without_previous_or_instructions_is_just_the_conversation() {
    let request = summary_request(None, &[Message::user("only")], None);
    let body = request.messages[0].text();
    assert!(body.contains("only"), "{body}");
    assert!(!body.contains("<previous-summary>"), "{body}");
    assert!(!body.contains("Additional instructions"), "{body}");
}

// ---------- check_summary ----------

#[test]
fn check_summary_rejects_empty_and_overlong() {
    assert_eq!(check_summary("  \n\t ", 100), Err(SummaryError::Empty));
    let long = "x".repeat(100_000);
    assert!(matches!(
        check_summary(&long, 100),
        Err(SummaryError::TooLong { tokens, max }) if tokens > 100 && max == 100
    ));
    assert_eq!(check_summary("  hello  ", 100).unwrap(), "hello");
}

// ---------- compacted_history ----------

#[test]
fn the_summary_message_carries_the_system_source_and_origin() {
    let kept = vec![Message::user("next")];
    let history = compacted_history("the summary", &kept).unwrap();
    assert_eq!(history.len(), 2);
    let Message::User { meta, .. } = &history[0] else {
        panic!("the summary must be a user message");
    };
    assert_eq!(meta.source, Some(InputSource::System));
    assert_eq!(
        meta.origin.as_ref().unwrap().get(),
        r#"{"kind":"compaction_summary"}"#
    );
    assert!(
        history[0].text().contains("the summary"),
        "{:?}",
        history[0]
    );
    assert_eq!(history[1], kept[0], "kept messages are unchanged");
}

#[test]
fn compacted_history_refuses_an_invalid_kept_suffix() {
    // A call with no result must not be committed as part of a compacted history.
    assert!(compacted_history("s", &[assistant_calls(&["a"])]).is_err());
    // A result with no call is invalid too.
    assert!(compacted_history("s", &[tool("a")]).is_err());
}

// ---------- randomized validity ----------

/// A tiny deterministic PRNG (xorshift64), so the test needs no dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A valid history with 4..=8 units, each message carrying enough text that the whole history
/// is over `K = 2048` for the `input_budget` the test uses.
fn random_history(rng: &mut Rng) -> Vec<Message> {
    let mut history = Vec::new();
    let units = 4 + rng.below(5);
    for unit in 0..units {
        let filler = "x".repeat(3_000);
        if rng.below(3) == 0 {
            history.push(Message::user(format!("u{unit} {filler}")));
            continue;
        }
        let calls = rng.below(4);
        if calls == 0 {
            history.push(Message::assistant(format!("a{unit} {filler}")));
            continue;
        }
        let ids: Vec<String> = (0..calls).map(|i| format!("c{unit}-{i}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        history.push(assistant_calls(&refs));
        for id in &ids {
            if rng.below(4) == 0 {
                history.push(tool_interrupted(id));
            } else {
                history.push(tool_with_text(id, &filler));
            }
        }
    }
    history
}

#[test]
fn plan_and_compaction_are_valid_for_many_histories() {
    for seed in 1..=200u64 {
        let mut rng = Rng::new(seed);
        let history = random_history(&mut rng);
        let plan = plan(&history, 8_192, &message_tokens)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        assert!(plan.first_kept > 0, "seed {seed}: nothing was summarized");
        let ranges = units(&history).unwrap();
        assert!(
            ranges.iter().any(|unit| unit.start == plan.first_kept),
            "seed {seed}: the cut is not a unit boundary: {plan:?}"
        );
        let kept = &history[plan.first_kept..];
        let compacted = compacted_history("summary", kept)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        assert_eq!(compacted.len(), kept.len() + 1, "seed {seed}");
    }
}
