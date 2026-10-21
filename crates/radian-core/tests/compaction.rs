//! Automatic compaction's pure planning half (ADR 0018): `keep_target`, `units`, `plan`,
//! `summary_request`, `check_summary` and `compacted_history`.

use radian_core::{
    ContentPart, InputSource, Message, MessageMeta, PlanError, SUMMARY_FACTS_DELETE_OLDEST,
    SUMMARY_FACTS_ORDER_RULE, SUMMARY_PLANS_ARE_NOT_FACTS, SummaryError, TOOL_RESULT_LIMIT_CHARS,
    ToolCall, ToolResult, check_summary, compacted_history, deterministic_summary,
    deterministic_summary_line, keep_target, message_tokens, plan, shorten_request, shorten_target,
    summary_max_tokens, summary_request, summary_system_prompt, truncate_kept_tool_results,
    truncate_tool_result_text, units, verified_facts_max_tokens,
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
    let request = summary_request(
        Some("PREVIOUS"),
        &history,
        Some("focus on the API"),
        8_192,
        Value::Null,
    );

    assert!(request.tools.is_empty(), "the summarizer gets no tools");
    assert_eq!(request.settings, Value::Null);
    let system = request.system_prompt.as_deref().unwrap();
    for section in [
        "## Goal",
        "## Constraints",
        "## Progress",
        "### Done",
        "### In progress",
        "### Pending",
        "## Verified facts (do not re-check)",
        "## Current work",
        "## Next step",
        "## Key decisions",
        "## Critical context",
        "## Files read and modified",
    ] {
        assert!(
            system.contains(section),
            "the prompt lacks {section}: {system}"
        );
    }
    // dogfood9 / plan B: the `Verified facts` section is restricted to what the conversation
    // actually checked; plans do not count. The test uses the stable substring constant, not
    // the whole paragraph.
    assert!(
        system.contains(SUMMARY_PLANS_ARE_NOT_FACTS),
        "the prompt does not exclude plans from Verified facts: {system}"
    );
    // dogfood9 follow-up: a concrete deletion priority and a chronological order, so "delete
    // the oldest" is executable by a model that cannot count tokens.
    assert!(system.contains(SUMMARY_FACTS_ORDER_RULE), "{system}");
    assert!(system.contains(SUMMARY_FACTS_DELETE_OLDEST), "{system}");
    // `Current work` names the file/function and `Next step` is a single continuation.
    assert!(
        system.contains("down to the file and the function"),
        "{system}"
    );
    assert!(system.contains("Exactly one step"), "{system}");

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

/// dogfood7: the model wrote a 10k-token summary because the prompt never stated the limit.
/// The system prompt now carries the same ceiling `check_summary` enforces.
#[test]
fn the_summary_request_states_the_length_ceiling() {
    let request = summary_request(None, &[Message::user("only")], None, 8_192, Value::Null);
    let system = request.system_prompt.as_deref().unwrap();
    assert!(system.contains("8192 tokens"), "{system}");
    // 1 token ≈ 0.75 words, so 8192 tokens is about 6144 words; the prompt says it is
    // approximate.
    assert!(system.contains("6144 English words"), "{system}");
    assert!(
        system.contains("shorten it, do not only append"),
        "{system}"
    );
    // A different ceiling produces a different prompt.
    let smaller = summary_request(None, &[Message::user("only")], None, 2_048, Value::Null);
    assert!(
        smaller
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("2048 tokens")
    );
}

#[test]
fn the_shorten_request_carries_only_the_overlong_summary() {
    let target = shorten_target(8_192);
    let request = shorten_request("OVERLONG SUMMARY BODY", target, Value::Null);
    let system = request.system_prompt.as_deref().unwrap();
    // 80% of 8192 is 6553.
    assert!(system.contains("6553 tokens"), "{system}");
    let body = request.messages[0].text();
    assert!(body.contains("OVERLONG SUMMARY BODY"), "{body}");
    // No fixed count ("seven").
    assert!(!body.contains("seven"), "{body}");
    // The deletion priority and the chronological order are stated in the retry too.
    assert!(body.contains(SUMMARY_FACTS_ORDER_RULE), "{body}");
    assert!(body.contains(SUMMARY_FACTS_DELETE_OLDEST), "{body}");
    // The retry names the same one-third ceiling for `Verified facts` (`target / 3`).
    assert_eq!(verified_facts_max_tokens(target), 2_184);
    assert!(body.contains("2184 tokens"), "{body}");
    assert!(body.contains("1638 English words"), "{body}");
    // The old contradiction ("must survive" / "never delete") is gone from both.
    for text in [system, body.as_str()] {
        assert!(!text.contains("must survive"), "{text}");
        assert!(!text.contains("never delete"), "{text}");
    }
    assert!(!body.contains("Conversation to summarize"), "{body}");
}

/// ADR 0018: the host's request-level compaction setting is copied
/// verbatim into the summary request and the shorten-retry request. The pure builders are the
/// one place both carry it, so both are pinned here.
#[test]
fn the_compaction_setting_is_copied_into_the_summary_and_the_shorten_requests() {
    let settings = json!({"reasoning_effort": "low"});
    let summary = summary_request(
        None,
        &[Message::user("only")],
        None,
        8_192,
        settings.clone(),
    );
    assert_eq!(summary.settings, settings);
    let shorten = shorten_request("OVERLONG", shorten_target(8_192), settings.clone());
    assert_eq!(shorten.settings, settings);
    // `Null` (the default) stays `Null`: no setting is invented.
    assert_eq!(
        summary_request(None, &[Message::user("only")], None, 8_192, Value::Null).settings,
        Value::Null
    );
}

/// The `Verified facts` section has its own named ceiling (one third), stated with the same
/// token + word ruler as the whole summary.
#[test]
fn the_summary_prompt_states_the_verified_facts_ceiling() {
    let max = 12_952;
    assert_eq!(verified_facts_max_tokens(max), 4_317);
    let prompt = summary_system_prompt(max);
    assert!(prompt.contains("4317 tokens"), "{prompt}");
    assert!(prompt.contains("3237 English words"), "{prompt}");
    assert!(!prompt.contains("must survive"), "{prompt}");
    assert!(!prompt.contains("never delete"), "{prompt}");
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
    let body = summary_request(None, &history, None, 8_192, Value::Null).messages[0].text();
    assert!(body.contains("called write w with"), "{body}");
    assert!(body.contains("[truncated"), "{body}");
    assert!(body.matches('y').count() < 2_000, "{body}");
}

#[test]
fn truncating_a_tool_result_never_splits_a_character() {
    // 2500 Han characters: a byte-based cut at 2000 would split one.
    let long = "中".repeat(2_500);
    let history = vec![assistant_calls(&["a"]), tool_with_text("a", &long)];
    let request = summary_request(None, &history, None, 8_192, Value::Null);
    let body = request.messages[0].text();
    assert!(body.contains("truncated 500 characters"), "{body}");
    assert_eq!(body.matches('中').count(), 2_000, "{body}");
}

#[test]
fn a_summary_request_without_previous_or_instructions_is_just_the_conversation() {
    let request = summary_request(None, &[Message::user("only")], None, 8_192, Value::Null);
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
    let history = compacted_history("the summary", &[], &kept).unwrap();
    assert_eq!(history.len(), 2);
    let Message::User { meta, .. } = &history[0] else {
        panic!("the summary must be a user message");
    };
    assert_eq!(meta.source, Some(InputSource::System));
    let origin = meta.origin.as_ref().unwrap().get().to_string();
    assert!(
        origin.contains(r#""kind":"compaction_summary""#),
        "{origin}"
    );
    assert!(origin.contains(r#""user_messages":[]"#), "{origin}");
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
    assert!(compacted_history("s", &[], &[assistant_calls(&["a"])]).is_err());
    // A result with no call is invalid too.
    assert!(compacted_history("s", &[], &[tool("a")]).is_err());
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
        let compacted = compacted_history("summary", &[], kept)
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
        assert_eq!(compacted.len(), kept.len() + 1, "seed {seed}");
    }
}

/// dogfood9: the summary ceiling scales with the window, not a fixed 8192.
#[test]
fn the_summary_ceiling_scales_with_the_window() {
    // 120K window (B = 103616, the dogfood9 example): keep_target = 20000, B/8 = 12952, so
    // 12952 (the shortening retry still wrote 8725 tokens at the old 8192 ceiling).
    assert_eq!(summary_max_tokens(103_616), 12_952);
    // A smaller window keeps the keep target: keep_target(16000) = 4000.
    assert_eq!(summary_max_tokens(16_000), 4_000);
    // At B/8 == the default the min picks the default; below that it still does.
    assert_eq!(summary_max_tokens(65_536), 8_192);
    assert_eq!(summary_max_tokens(4_000), 2_048);
}

/// The scaled ceiling reaches both the prompt (what the model is told) and the check (what
/// the core enforces): a summary between the old 8192 and the scaled 12952 passes only the
/// scaled one.
#[test]
fn the_scaled_ceiling_reaches_the_prompt_and_the_check() {
    let max = summary_max_tokens(103_616);
    let prompt = radian_core::summary_system_prompt(max);
    assert!(prompt.contains("12952 tokens"), "{prompt}");

    // ~10000 tokens by the default 4-bytes-per-token ruler: over the old default, under the
    // scaled one.
    let text = "a".repeat(40_000);
    let tokens = message_tokens(&Message::user(&text));
    assert!(tokens > 8_192 && tokens <= max, "{tokens}");
    assert!(
        check_summary(&text, 8_192).is_err(),
        "the old fixed ceiling would reject it"
    );
    assert_eq!(check_summary(&text, max).unwrap().len(), text.len());
}

// ---------- ADR 0018: the user's words, kept verbatim ----------

/// A user message with `source = source` and one text part.
fn user_sourced(text: &str, source: Option<InputSource>) -> Message {
    Message::User {
        content: vec![ContentPart::text(text)],
        meta: MessageMeta {
            source,
            ..MessageMeta::default()
        },
    }
}

fn texts(kept: &[radian_core::RetainedUserMessage]) -> Vec<String> {
    kept.iter().map(|message| message.text.clone()).collect()
}

/// The budget is the ADR's two-layer rule, including both bounds and the no-window fallback.
#[test]
fn the_verbatim_budget_is_the_two_layer_rule() {
    use radian_core::user_message_budget;
    // A small window: the 512 floor.
    assert_eq!(user_message_budget(Some(4_000)), (512, 512));
    // A medium window: B/16, and U₁ shrinks with U.
    assert_eq!(user_message_budget(Some(16_000)), (1_000, 1_000));
    assert_eq!(user_message_budget(Some(64_000)), (4_000, 2_000));
    // A large window: the 8192 ceiling, U₁ capped at 2000.
    assert_eq!(user_message_budget(Some(400_000)), (8_192, 2_000));
    // No window: the fixed pair.
    assert_eq!(user_message_budget(None), (8_192, 2_000));
}

/// Only `User` (and a missing source, at this layer) is retained; every other source is not.
#[test]
fn only_user_messages_are_retained() {
    use radian_core::select_user_messages;
    let messages = vec![
        user_sourced("from the user", Some(InputSource::User)),
        user_sourced("no source at all", None),
        user_sourced("a slash command", Some(InputSource::SlashCommand)),
        user_sourced("from the scheduler", Some(InputSource::Scheduler)),
        user_sourced("from mcp", Some(InputSource::Mcp)),
        user_sourced("from a subagent", Some(InputSource::Subagent)),
        user_sourced("system", Some(InputSource::System)),
        user_sourced(
            "a permission decision",
            Some(InputSource::PermissionDecision),
        ),
        Message::assistant("an assistant turn"),
    ];
    let kept = select_user_messages(&[], &messages, None);
    assert_eq!(
        texts(&kept),
        ["from the user", "no source at all"],
        "only the human's words are kept"
    );
}

/// The summary message itself is a `System`-sourced user message, so a second compaction never
/// re-retains it.
#[test]
fn the_summary_message_is_not_retained() {
    use radian_core::{compacted_history, select_user_messages};
    let history = compacted_history("a summary", &[], &[]).unwrap();
    let kept = select_user_messages(&[], &history, None);
    assert!(kept.is_empty(), "{kept:?}");
}

/// An attached file is a placeholder, never its body.
#[test]
fn an_attachment_becomes_a_placeholder() {
    use radian_core::select_user_messages;
    let body = "secret\nfile\ncontents";
    let message = Message::User {
        content: vec![
            ContentPart::text("look at @notes.txt"),
            ContentPart::attachment(body, "notes.txt"),
        ],
        meta: MessageMeta {
            source: Some(InputSource::User),
            ..MessageMeta::default()
        },
    };
    let kept = select_user_messages(&[], &[message], None);
    assert_eq!(kept.len(), 1);
    assert_eq!(
        kept[0].text,
        "look at @notes.txt\n[file: notes.txt, 3 lines]"
    );
    assert!(!kept[0].text.contains("secret"), "{:?}", kept[0].text);
}

/// The first message is always kept, and over `U₁` it is cut with the middle marked.
#[test]
fn the_first_message_is_kept_and_cut_to_u1() {
    use radian_core::{select_user_messages, user_message_budget};
    let first = format!("HEAD{}TAIL", "x".repeat(40_000));
    let messages = vec![user_sourced(&first, Some(InputSource::User))];
    let (_, u1) = user_message_budget(Some(400_000));
    let kept = select_user_messages(&[], &messages, Some(400_000));
    assert_eq!(kept.len(), 1);
    assert!(kept[0].text.starts_with("HEAD"), "{:?}", kept[0].text);
    assert!(kept[0].text.ends_with("TAIL"), "{:?}", kept[0].text);
    assert!(
        kept[0].text.contains("tokens omitted"),
        "{:?}",
        kept[0].text
    );
    assert!(
        message_tokens(&Message::user(&kept[0].text)) <= u1,
        "the cut must fit U₁"
    );
}

/// After the first, the messages fill `U` from the newest backwards, and the ones that did not
/// fit are counted. The output is chronological.
#[test]
fn the_rest_fill_u_from_the_newest_with_the_omitted_count() {
    use radian_core::select_user_messages;
    // A window that fits the first and about three of the ten messages.
    let budget = Some(4_000);
    let mut messages = vec![user_sourced("the task", Some(InputSource::User))];
    for index in 0..10 {
        messages.push(user_sourced(
            &format!("message {index} {}", "y".repeat(600)),
            Some(InputSource::User),
        ));
    }
    let kept = select_user_messages(&[], &messages, budget);
    let shown = texts(&kept);
    assert_eq!(shown[0], "the task", "the first stays first");
    assert!(
        shown
            .iter()
            .any(|text| text.contains("earlier user messages omitted")),
        "{shown:?}"
    );
    // Chronological: the messages that survive are in their original order.
    let survivors: Vec<&String> = shown
        .iter()
        .filter(|text| text.starts_with("message "))
        .collect();
    let mut sorted = survivors.clone();
    sorted.sort();
    assert_eq!(survivors, sorted, "{shown:?}");
    // The newest is always there.
    assert!(
        shown.iter().any(|text| text.starts_with("message 9")),
        "the newest must always be kept: {shown:?}"
    );
    // The messages after the first together cost at most U.
    let (u, _) = radian_core::user_message_budget(budget);
    let used: u64 = shown[1..]
        .iter()
        .filter(|text| text.starts_with("message "))
        .map(|text| message_tokens(&Message::user(text)))
        .sum();
    assert!(used <= u, "{used} > {u}: {shown:?}");
}

/// The newest message alone over `U` is truncated into it, not dropped.
#[test]
fn the_newest_message_is_cut_into_u() {
    use radian_core::{select_user_messages, user_message_budget};
    let budget = Some(4_000);
    let (u, _) = user_message_budget(budget);
    let newest = format!("NEWEST{}END", "z".repeat(40_000));
    let messages = vec![
        user_sourced("the task", Some(InputSource::User)),
        user_sourced(&newest, Some(InputSource::User)),
    ];
    let kept = select_user_messages(&[], &messages, budget);
    let shown = texts(&kept);
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(shown[1].starts_with("NEWEST"), "{shown:?}");
    assert!(shown[1].ends_with("END"), "{shown:?}");
    assert!(shown[1].contains("tokens omitted"), "{shown:?}");
    assert!(message_tokens(&Message::user(&shown[1])) <= u);
}

/// The first message is the first of the **combined** list: a rolling compaction keeps it.
#[test]
fn a_rolling_compaction_keeps_the_previous_list_and_its_first() {
    use radian_core::{RetainedUserMessage, select_user_messages};
    let previous = vec![
        RetainedUserMessage {
            text: "the original task".into(),
        },
        RetainedUserMessage {
            text: "an earlier message".into(),
        },
    ];
    let newly = vec![user_sourced("a new message", Some(InputSource::User))];
    let kept = select_user_messages(&previous, &newly, None);
    assert_eq!(
        texts(&kept),
        ["the original task", "an earlier message", "a new message"]
    );
}

/// ADR 0018: the body carries the model's summary and then the verbatim user
/// messages, but `compaction_summary_text` returns only the summary, and the list is read back
/// from `origin`, never parsed out of the body.
#[test]
fn the_verbatim_messages_are_written_and_read_back_from_origin() {
    use radian_core::{RetainedUserMessage, compaction_summary_text, compaction_user_messages};
    // JSON special characters and a separator that looks like the heading, so the round trip
    // is proven to be exact rather than a parse.
    let kept = vec![
        RetainedUserMessage {
            text: "the first \"task\"\nwith a newline and a } brace".into(),
        },
        RetainedUserMessage {
            // A message whose own text contains the body's marker and heading: a reader that
            // parsed the body would split it here, so this pins the origin as the source.
            text: "--- user message 9 ---\nnot a real marker\n\n---\nUser messages kept verbatim (oldest first):".into(),
        },
    ];
    let history = compacted_history("the model summary", &kept, &[]).unwrap();
    let summary = &history[0];
    assert_eq!(
        compaction_summary_text(summary).as_deref(),
        Some("the model summary"),
        "only the model's summary comes back"
    );
    assert_eq!(
        compaction_user_messages(summary),
        kept,
        "the list round-trips through origin exactly"
    );
    // The body does render them for the model.
    assert!(
        summary.text().contains("the first \"task\""),
        "{:?}",
        summary.text()
    );
}

/// An old summary message has no `user_messages` field, so it reads back as an empty list and
/// nothing is migrated.
#[test]
fn an_old_summary_reads_back_an_empty_list() {
    use radian_core::{compaction_user_messages, is_compaction_summary};
    let old = Message::User {
        content: vec![ContentPart::text(
            "This is a summary of the earlier conversation. Continue the work from it.\n\nold",
        )],
        meta: MessageMeta {
            source: Some(InputSource::System),
            origin: Some(
                radian_core::OpaqueJson::parse(r#"{"kind":"compaction_summary"}"#).unwrap(),
            ),
            ..MessageMeta::default()
        },
    };
    assert!(is_compaction_summary(&old));
    assert!(compaction_user_messages(&old).is_empty());
}

/// A rolling compaction replaces the omission marker instead of accumulating one per round.
#[test]
fn a_second_compaction_does_not_stack_omission_markers() {
    use radian_core::{omitted_marker, select_user_messages};
    let budget = Some(4_000);
    let mut first = vec![user_sourced("the task", Some(InputSource::User))];
    for index in 0..8 {
        first.push(user_sourced(
            &format!("message {index} {}", "y".repeat(600)),
            Some(InputSource::User),
        ));
    }
    let kept = select_user_messages(&[], &first, budget);
    let markers = texts(&kept)
        .iter()
        .filter(|text| text.contains("earlier user messages omitted"))
        .count();
    assert_eq!(markers, 1, "{:?}", texts(&kept));

    // The next round: `kept` is the previous list, and one new message arrives.
    let next = vec![user_sourced("a new message", Some(InputSource::User))];
    let rolled = select_user_messages(&kept, &next, budget);
    let markers = texts(&rolled)
        .iter()
        .filter(|text| text.contains("earlier user messages omitted"))
        .count();
    assert_eq!(markers, 1, "{:?}", texts(&rolled));
    assert_eq!(texts(&rolled)[0], "the task", "the first stays first");
    assert_eq!(
        texts(&rolled).last().map(String::as_str),
        Some("a new message"),
        "the newest is kept"
    );
    assert_eq!(omitted_marker(1), "[1 earlier user messages omitted]");
}

/// ADR 0018: the omission count accumulates across rolling compactions — the new
/// marker names everything that is still missing, not just this round's drops — and a round
/// that drops nothing keeps the previous count instead of losing the marker.
#[test]
fn the_omission_count_accumulates() {
    use radian_core::select_user_messages;

    // A window that fits the first message and one truncated long one, and nothing else.
    let budget = Some(4_000);
    let long = |index: usize| {
        user_sourced(
            &format!("long {index} {}", "y".repeat(2_000)),
            Some(InputSource::User),
        )
    };
    let marker_count = |kept: &[radian_core::RetainedUserMessage]| -> Option<usize> {
        kept.iter().find_map(|message| {
            message
                .text
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(" earlier user messages omitted]"))
                .and_then(|count| count.parse().ok())
        })
    };

    // Round 1: four long messages. The newest is truncated into `U`; the other three do not
    // fit and are counted.
    let mut round1 = vec![user_sourced("the task", Some(InputSource::User))];
    for index in 0..4 {
        round1.push(long(index));
    }
    let first = select_user_messages(&[], &round1, budget);
    assert_eq!(marker_count(&first), Some(3), "{:?}", texts(&first));

    // A round with no new messages keeps the count (the bug dropped the marker entirely).
    let same = select_user_messages(&first, &[], budget);
    assert_eq!(marker_count(&same), Some(3), "{:?}", texts(&same));

    // Two more long messages: the marker names all five.
    let more = vec![long(4), long(5)];
    let second = select_user_messages(&first, &more, budget);
    assert_eq!(marker_count(&second), Some(5), "{:?}", texts(&second));
}

/// ADR 0018: the kept messages after the first are the
/// **contiguous** newest run. A short message older than one that did not fit is not kept just
/// because it would have fit.
#[test]
fn the_selection_is_the_contiguous_newest_run() {
    use radian_core::select_user_messages;

    let budget = Some(4_000);
    let short = |index: usize| user_sourced(&format!("short {index}"), Some(InputSource::User));
    let messages = vec![
        short(0), // the first: always kept
        short(1), // older than the long one, and short enough to fit
        user_sourced(&"x".repeat(2_000), Some(InputSource::User)),
        short(2),
        short(3), // the newest
    ];
    let kept = select_user_messages(&[], &messages, budget);
    assert_eq!(
        texts(&kept),
        [
            "short 0",
            "[2 earlier user messages omitted]",
            "short 2",
            "short 3"
        ],
        "the long one and everything older are omitted"
    );
}

/// ADR 0018: [`select_user_messages_tracked`] returns, next to the list,
/// how many of the newly summarized user messages that list carries. The deterministic fallback
/// subtracts that count from the removed prefix to report `N`, so it must count only the new
/// messages (not the previous list's) and only the carried ones (not the omitted).
#[test]
fn the_tracked_count_names_only_carried_new_messages() {
    use radian_core::{RetainedUserMessage, select_user_messages_tracked};

    // Everything fits: both new user messages are carried.
    let budget = Some(16_000); // U = U₁ = 1000
    let messages = vec![
        user_sourced("task", Some(InputSource::User)),
        user_sourced("next", Some(InputSource::User)),
    ];
    let (kept, carried) = select_user_messages_tracked(&[], &messages, budget);
    assert_eq!(texts(&kept), ["task", "next"]);
    assert_eq!(carried, 2);

    // The previous list's own entry is carried but is not one of the new messages.
    let previous = vec![RetainedUserMessage { text: "old".into() }];
    let (kept, carried) = select_user_messages_tracked(&previous, &messages, budget);
    assert_eq!(texts(&kept), ["old", "task", "next"]);
    assert_eq!(carried, 2, "the previous entry is not a new message");

    // An omitted new message is not carried.
    let many = vec![
        user_sourced("first", Some(InputSource::User)),
        user_sourced(&"x".repeat(8_000), Some(InputSource::User)), // over `U`, not the newest
        user_sourced("newest", Some(InputSource::User)),
    ];
    let (kept, carried) = select_user_messages_tracked(&[], &many, budget);
    assert_eq!(
        texts(&kept),
        ["first", "[1 earlier user messages omitted]", "newest"]
    );
    assert_eq!(carried, 2, "the omitted one is not carried");
}

// ---------- ADR 0018: the deterministic fallback ----------

/// The truncation keeps the marker **inside** the limit, so a kept tool result never grows past
/// `TOOL_RESULT_LIMIT_CHARS`; a result that already fits is untouched.
#[test]
fn a_truncated_tool_result_keeps_the_marker_within_the_limit() {
    let long = "x".repeat(TOOL_RESULT_LIMIT_CHARS + 5_000);
    let cut = truncate_tool_result_text(&long).expect("over the limit");
    assert!(
        cut.chars().count() <= TOOL_RESULT_LIMIT_CHARS,
        "the marker must fit, got {} characters",
        cut.chars().count()
    );
    assert!(cut.contains("[truncated"), "{cut}");
    assert!(cut.starts_with("xxx"), "{cut}");

    // A result that already fits is left as it is.
    assert_eq!(truncate_tool_result_text("short"), None);

    // Cutting is by `char`: a multi-byte character is never split.
    let wide = "é".repeat(TOOL_RESULT_LIMIT_CHARS + 10);
    let cut = truncate_tool_result_text(&wide).expect("over the limit");
    assert!(
        cut.chars()
            .all(|c| c == 'é' || !c.is_alphanumeric() || c.is_ascii())
    );
    assert!(cut.chars().count() <= TOOL_RESULT_LIMIT_CHARS);
}

/// `truncate_kept_tool_results` cuts only the over-long results and counts them (the `M` in the
/// deterministic fixed line), leaving its other messages untouched.
#[test]
fn truncate_kept_tool_results_cuts_only_over_long_results() {
    let mut kept = vec![
        assistant_calls(&["c1", "c2"]),
        tool_with_text("c1", &"a".repeat(TOOL_RESULT_LIMIT_CHARS + 1)),
        tool_with_text("c2", "short"),
    ];
    assert_eq!(truncate_kept_tool_results(&mut kept), 1);
    let Message::Tool { result, .. } = &kept[1] else {
        unreachable!()
    };
    assert!(result.text().chars().count() <= TOOL_RESULT_LIMIT_CHARS);
    let Message::Tool { result, .. } = &kept[2] else {
        unreachable!()
    };
    assert_eq!(result.text(), "short");
    assert!(matches!(kept[0], Message::Assistant { .. }));
}

/// The deterministic summary is the previous model summary verbatim (trimmed) plus the fixed
/// line naming what was dropped and cut.
#[test]
fn the_deterministic_summary_keeps_the_previous_summary_verbatim() {
    assert_eq!(
        deterministic_summary(None, 3, 2),
        deterministic_summary_line(3, 2)
    );
    let line = deterministic_summary_line(3, 2);
    assert!(line.contains("3 messages dropped"), "{line}");
    assert!(line.contains("2 tool results truncated"), "{line}");

    let with = deterministic_summary(Some("  the old summary  "), 0, 0);
    assert!(with.starts_with("the old summary"), "{with}");
    assert!(with.ends_with(&deterministic_summary_line(0, 0)), "{with}");

    // A blank previous summary is treated as none.
    assert_eq!(
        deterministic_summary(Some("   "), 1, 0),
        deterministic_summary_line(1, 0)
    );
}
