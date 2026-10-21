//! The built-in intent-stop validator (backlog `turn-end-continuation.md` §B): the
//! conservative rule that spots a turn ending with "announcing a next step but making
//! no tool call", the fixed continuation text, and the `origin` that audits it.
//!
//! The validator runs on the `Stop` continuation path (`CommandHooks::run_stop_once`),
//! under the same wake gate and degrade, but it is not a user hook: it fires on the
//! rule below, at most once per turn, at most twice in a row (the third consecutive
//! hit reports `blocked` instead of continuing).
//!
//! The rule is a coarse sieve on purpose: a false positive costs one short model call
//! and one transcript line (the model judges whether to continue), never an unplanned
//! action. The fixed sample in `intent_stop_sample.json` pins its behavior — every
//! entry is a real turn-ending message from this machine's journals, scrubbed only of
//! session ids, pane ids and absolute paths (lengths of the long ones stay far above
//! the limit either way).

use radian_core::{Message, OpaqueJson};

/// How long the last assistant message may be, in characters, for the validator to
/// consider it. About 200 (the spec's "约 200"): every true stall on this machine ends
/// in at most 112 characters except two long lead-session reports, which this limit
/// knowingly misses (they stay in the sample as `hold`, the documented recall gap);
/// every named false-positive kind is far longer and stays out on length alone.
pub const INTENT_STOP_MAX_CHARS: usize = 200;

/// The fixed continuation text (backlog §B.2): the judgement stays with the model, so
/// a false positive costs a short call and one transcript line, never a plan.
pub const INTENT_STOP_TEXT: &str = "Your last message announced a next step but made no tool call. If the task is not done, continue now. If it is done, or you need the user, say so in one line.";

/// The TUI notice line for the two-in-a-row cap: no third continuation opens, herdr
/// reports `blocked` with the `intent stop` label, and sending any message continues.
pub fn intent_stop_cap_line() -> String {
    "intent stop: two continuations in a row ended the same way with no tool call — send any message to continue"
        .to_owned()
}

/// The `origin` auditing a validator-driven continuation (ADR 0005): validator-driven
/// followups carry `{"continuation":"intent-stop"}`, the loop's shape; hook-driven
/// ones keep their existing unmarked shape.
pub fn intent_stop_origin() -> OpaqueJson {
    OpaqueJson::parse(r#"{"continuation":"intent-stop"}"#).expect("the literal is valid JSON")
}

/// The validator's continuation message: user role (the loop's shape) with the
/// validator origin; the `System` source is attached at the inbox boundary.
pub fn intent_stop_message() -> Message {
    let mut message = Message::user(INTENT_STOP_TEXT);
    match &mut message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta.origin = Some(intent_stop_origin()),
    }
    message
}

/// Whether the inbox message is a validator-driven continuation.
pub fn is_intent_stop_continuation(message: &Message) -> bool {
    let meta = match message {
        Message::User { meta, .. }
        | Message::Assistant { meta, .. }
        | Message::Tool { meta, .. } => meta,
    };
    let Some(origin) = meta.origin.as_ref() else {
        return false;
    };
    let value: serde_json::Value = match serde_json::from_str(origin.get()) {
        Ok(value) => value,
        Err(_) => return false,
    };
    value.get("continuation").and_then(|kind| kind.as_str()) == Some("intent-stop")
}

/// The last sentence: the text after the final sentence end (`.!?。！？…`) or line
/// break. Only the last sentence is examined — the stall shape is always "…announcing
/// what comes next" at the very end.
pub fn last_sentence(text: &str) -> &str {
    let mut last = text.trim();
    let mut begin = 0;
    while begin < text.len() {
        let Some(offset) = text[begin..].find(['.', '!', '?', '。', '！', '？', '…', '\n', '\r'])
        else {
            break;
        };
        let cut = begin + offset;
        // Step past the separator (possibly multibyte) and keep the last non-empty tail.
        let next = cut + text[cut..].chars().next().map_or(1, |ch| ch.len_utf8());
        let piece = text[next..].trim();
        if !piece.is_empty() {
            last = piece;
        }
        begin = next;
    }
    last
}

/// The conservative rule (backlog §B.1): a text-only assistant message with no tool
/// calls, at most [`INTENT_STOP_MAX_CHARS`] characters, whose last sentence ends in
/// no question mark, asks nothing of the user, opens with a gerund / `Now` / `Next` /
/// `Then` / `Let me` / `I'll` / `I will` / `Time to` / 接下来 / 现在 / 然后 / 下一步
/// (`Nothing` is not a gerund), and whose subject is not `you` / `p1` / `lead` / 你.
pub fn matches_rule(message: &Message) -> bool {
    let Message::Assistant {
        content,
        tool_calls,
        ..
    } = message
    else {
        return false;
    };
    if !tool_calls.is_empty() {
        return false;
    }
    let text: String = content
        .iter()
        .filter_map(|part| match part {
            radian_core::ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.trim().is_empty() || text.chars().count() > INTENT_STOP_MAX_CHARS {
        return false;
    }
    let last = last_sentence(&text);
    if last.is_empty() || last.ends_with(['?', '？']) {
        return false;
    }
    if ["要不要", "说一声", "告诉我", "需要我"]
        .iter()
        .any(|ask| last.contains(ask))
    {
        return false;
    }
    let Some(rest) = opening_rest(last) else {
        return false;
    };
    !is_other_subject(rest)
}

/// Strips a recognized opening from the last sentence, returning the remainder.
/// `None` when the sentence opens with none of the listed forms.
fn opening_rest(sentence: &str) -> Option<&str> {
    // The Chinese markers first (byte prefixes, no boundary needed).
    for marker in ["接下来", "现在", "然后", "下一步"] {
        if let Some(rest) = sentence.strip_prefix(marker) {
            return Some(rest);
        }
    }
    // The English phrase markers, with a word boundary (a hyphenated token is one
    // token, not a marker opening: "Now-or-never" holds).
    for marker in ["Let me", "I will", "Time to", "I'll", "Now", "Next", "Then"] {
        if let Some(rest) = sentence.strip_prefix(marker)
            && rest
                .chars()
                .next()
                .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '-')
        {
            return Some(rest);
        }
    }
    // A gerund: the first word ends in "ing" — except "Nothing", which only looks
    // like one ("Nothing further unless p1 wants changes."). A word boundary follows:
    // a hyphenated token ("THINKING-OK") is one token, not a gerund opening.
    let word: String = sentence
        .chars()
        .take_while(|ch| ch.is_ascii_alphabetic())
        .collect();
    if word.len() > "ing".len()
        && word.to_ascii_lowercase().ends_with("ing")
        && !word.eq_ignore_ascii_case("nothing")
        && sentence[word.len()..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_ascii_alphanumeric() && next != '-')
    {
        return Some(&sentence[word.len()..]);
    }
    None
}

/// Whether the remainder after the opening names somebody else as the actor: it
/// starts with `you` / `p1` / `lead` (the first word, case-insensitive) or with 你.
/// "Notifying the lead's pane." stays: `lead` there is the recipient, not the actor —
/// only the subject position counts.
fn is_other_subject(rest: &str) -> bool {
    let rest = rest.trim_start_matches([':', '：', ',', '，', '、', '.', '。', ' ']);
    if rest.starts_with('你') {
        return true;
    }
    let word: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    matches!(word.to_ascii_lowercase().as_str(), "you" | "p1" | "lead")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One fixed-sample entry: a real turn-ending message and the rule's verdict.
    #[derive(Debug, serde::Deserialize)]
    struct Sample {
        key: String,
        label: String,
        text: String,
    }

    /// The fixed sample (backlog §B acceptance): the stalls this machine's journals
    /// hold plus the false-positive kinds and ordinary text-only turn ends. Every
    /// label is the rule's expected verdict — including the two long lead reports,
    /// which the length limit knowingly misses (`hold`, the documented recall gap),
    /// and the two short waits, which the pure rule fires on (`fire`; the running-job
    /// suppression that holds them in production is pinned in `hooks.rs` instead).
    #[test]
    fn the_fixed_sample_labels_hold() {
        let samples: Vec<Sample> = serde_json::from_str(include_str!("intent_stop_sample.json"))
            .expect("the sample is valid JSON");
        assert_eq!(samples.len(), 40, "the sample keeps its size: {samples:?}");
        let mut fires = 0;
        for sample in &samples {
            let verdict = matches_rule(&Message::assistant(&sample.text));
            let expected = sample.label == "fire";
            fires += usize::from(verdict);
            assert_eq!(
                verdict, expected,
                "{}: expected {}, the rule says otherwise: {:?}",
                sample.key, sample.label, sample.text
            );
        }
        assert_eq!(fires, 11, "the sample keeps its fire count");
    }

    /// Non-assistant messages never match, and neither does an empty one.
    #[test]
    fn only_non_empty_text_assistant_messages_match() {
        assert!(!matches_rule(&Message::user(
            "Now the docs-only ADR updates."
        )));
        assert!(!matches_rule(&Message::assistant("")));
        assert!(!matches_rule(&Message::assistant("   ")));
    }

    /// Any tool call disqualifies: "announcing then calling" is the healthy shape.
    #[test]
    fn a_tool_call_disqualifies() {
        let message = Message::Assistant {
            content: vec![radian_core::ContentPart::text(
                "Now the docs-only ADR updates.",
            )],
            reasoning: Vec::new(),
            tool_calls: vec![radian_core::ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: serde_json::json!({}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        };
        assert!(!matches_rule(&message));
    }

    /// A question mark (either language) or a ask-the-user phrase holds the rule.
    #[test]
    fn questions_and_ask_phrases_hold() {
        for text in [
            "Done. Any questions?",
            "需要我动手做哪一项吗？我建议先处理第 1 条。",
            "读到的前 60 行被截断了，需要看哪个模块就说一声。",
            "Tell me what you want done.",
        ] {
            assert!(!matches_rule(&Message::assistant(text)), "{text}");
        }
    }

    /// The named false-positive kinds hold each for its own reason.
    #[test]
    fn each_named_false_positive_kind_holds() {
        // A hyphenated token is one token, not a gerund or marker opening.
        assert!(!matches_rule(&Message::assistant("THINKING-OK")));
        assert!(!matches_rule(&Message::assistant("Now-or-never")));
        // "Nothing" is not a gerund.
        assert!(!matches_rule(&Message::assistant(
            "Nothing further unless p1 wants changes."
        )));
        // A suggestion to the user (long, and user-directed).
        let text = format!("{}\n\n现在重启你的 radian 就能试。", "y".repeat(300));
        assert!(!matches_rule(&Message::assistant(text)));
        // `you` as the actor.
        assert!(!matches_rule(&Message::assistant(
            "Next: you merge the branch."
        )));
        // A long report stays out on length even with a matching last sentence.
        let text = format!("{}\n\nNext: the reviewer decides.", "x".repeat(300));
        assert!(!matches_rule(&Message::assistant(text)));
    }

    /// The subject check is positional: `lead` as the recipient keeps firing.
    #[test]
    fn the_lead_as_recipient_still_fires() {
        assert!(matches_rule(&Message::assistant(
            "All checks passed. Notifying the lead's pane."
        )));
        assert!(!matches_rule(&Message::assistant(
            "Next: lead reviews the branch."
        )));
    }

    /// The continuation message carries the audit origin, and only it does.
    #[test]
    fn the_continuation_message_is_tagged_intent_stop() {
        let message = intent_stop_message();
        assert!(is_intent_stop_continuation(&message));
        assert_eq!(message.text(), INTENT_STOP_TEXT);
        assert!(!is_intent_stop_continuation(&Message::user("hi")));
        assert!(!is_intent_stop_continuation(&Message::assistant(
            "Now the docs-only ADR updates."
        )));
    }

    /// The length limit is on the whole message: a short last sentence in a long
    /// report holds.
    #[test]
    fn a_short_last_sentence_in_a_long_report_holds() {
        let text = format!("{}\n\n{}", "x".repeat(300), "Writing the report.");
        assert!(!matches_rule(&Message::assistant(text)));
    }
}
