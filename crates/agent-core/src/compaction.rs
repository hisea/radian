//! Automatic compaction: the **pure planning half** (ADR 0018).
//!
//! ADR 0018 defines automatic compaction from the context-budget projection (ADR 0015):
//! when the projected input crosses the effective input budget `B`, the conversation is
//! summarized and the request is retried once. This module is only the part that can be
//! decided without I/O: how much recent history to keep, which messages form one atomic
//! unit, what to ask the summarizer for, whether the answer is usable, and what the
//! compacted history looks like. The turn loop, the journal writes, the model call and the
//! retry live in `turn.rs::maybe_compact`; nothing here is `async` and nothing here touches a
//! file.
//!
//! The rules come straight from ADR 0018:
//!
//! * keep target `K = min(20000, max(2048, ⌊B/4⌋))` — a target, not a hard limit;
//! * cut only at **atomic units**: a user message, or an assistant message together with
//!   every tool result that answers it (ADR 0002/0003's repaired `interrupted` results
//!   included). A long tool chain can be split at one of its assistant messages;
//! * the compacted history is `summary + kept` and must pass the core's own
//!   `validate_history`, because a summary that breaks tool pairing would poison the next
//!   request.

use std::ops::Range;

use serde_json::Value;

use crate::budget::{self, DEFAULT_BYTES_PER_TOKEN};
use crate::message::{ContentPart, InputSource, Message, MessageMeta, OpaqueJson};
use crate::model::ModelRequest;
use crate::outcome::SessionError;
use crate::preflight::validate_history;

/// The largest keep target `K` (ADR 0018: `min(20000, …)`).
pub const MAX_KEEP_TARGET: u64 = 20_000;

/// The smallest keep target `K` (ADR 0018: `max(2048, …)`).
pub const MIN_KEEP_TARGET: u64 = 2_048;

/// The summarizer is the current conversation model (ADR 0018 `[compaction] model =
/// "current"`); this is the `model` value the compaction records carry.
pub const COMPACTION_MODEL_CURRENT: &str = "current";

/// The default summary length ceiling (ADR 0018 says the summary must be within a limit but
/// does not fix one; this slice fixes 8192 tokens, deliberately not `B`).
pub const DEFAULT_SUMMARY_MAX_TOKENS: u64 = 8_192;

/// How many characters of one tool result (ADR 0018), and of one tool call's arguments,
/// reach the summarizer.
pub const TOOL_RESULT_LIMIT_CHARS: usize = 2_000;

/// The rule for the `Verified facts` section: only
/// what the conversation actually verified. Plans and intentions are explicitly excluded. The
/// prompt quotes this sentence; the constant is the stable substring a test asserts, so the
/// wording can change without binding the test to the whole paragraph.
pub const SUMMARY_PLANS_ARE_NOT_FACTS: &str =
    "Plans, assumptions and intentions are not facts and must not appear here";

/// The rule that `Verified facts` is chronological (oldest first, new facts appended at the
/// end), so "delete the oldest first" has a defined meaning. Quoted in the summary prompt and
/// the shorten body; the stable substring a test asserts.
pub const SUMMARY_FACTS_ORDER_RULE: &str =
    "list the facts in chronological order, oldest first, and append each new fact at the end";

/// The rule for choosing which fact to delete when the section is over its ceiling: the
/// oldest one, and only when it is no longer relevant to the Goal and the current work. Quoted
/// in the summary prompt and the shorten body; the stable substring a test asserts.
pub const SUMMARY_FACTS_DELETE_OLDEST: &str = "delete the oldest fact first";

/// The fixed part of the summary system prompt (ADR 0018 §摘要, revision 5 / plan B). It asks
/// for the sections by name and forbids inventing anything, because the summary becomes part of
/// the next request's context. The length ceiling is added by [`summary_system_prompt`], which
/// depends on the compaction.
pub const SUMMARY_SYSTEM_PROMPT: &str = "\
You are compressing a coding conversation so another agent can continue the work. \
Produce one structured summary with exactly these sections, in this order:

## Goal
What the user asked for, in their own words where possible \u{2014} quote the user; do not only paraphrase.

## Constraints
The rules, limits and preferences that must hold.

## Progress
### Done
What is finished, with the exact commands and results.
### In progress
What is partly done.
### Pending
What is not started.

## Verified facts (do not re-check)
Only facts the conversation actually verified belong here: commands that ran with their results, and conclusions drawn from code that was read. Plans, assumptions and intentions are not facts and must not appear here. State how each fact was verified (which command, or which file and line), and do not re-check these facts later. Keep the section ordered: list the facts in chronological order, oldest first, and append each new fact at the end (a rolling update keeps that order). When the summary is over the length limit, first drop stale details from the other sections; delete a fact only when it is no longer relevant to the Goal and to the Pending or In progress work, and then delete the oldest fact first.

## Current work
The one thing being worked on right before this summary, down to the file and the function.

## Next step
Exactly one step, and it must continue directly from Current work.

## Key decisions
The decisions taken and why.

## Critical context
Anything else the next agent must know: the environment, gotchas, exact error text.

## Files read and modified
Every file touched, with its path.

Keep every section concise. Preserve exact file paths, function names, commands, and error \
messages. Include only what the conversation contains; never invent or guess. Output only the \
summary, with no preamble.";

/// The share of the ceiling the shortening retry aims for, leaving margin so the result
/// comfortably passes `check_summary`.
pub const SHORTEN_TARGET_PERCENT: u64 = 80;

/// The token ceiling the shortening retry asks for: 80% of the enforced limit.
pub fn shorten_target(max_tokens: u64) -> u64 {
    max_tokens.saturating_mul(SHORTEN_TARGET_PERCENT) / 100
}

/// The token ceiling for the `Verified facts` section **alone**: one third of the whole
/// summary ceiling. A named number, not just "delete by priority", because the model cannot
/// count tokens: when everything else is trimmed and the section
/// still overflows, the retry needs a concrete target to delete toward.
pub fn verified_facts_max_tokens(max_tokens: u64) -> u64 {
    max_tokens / 3
}

/// The word count the prompt quotes for a token ceiling. ADR 0018's ruler is 1 token ≈ 0.75
/// English words; the prompt says so, because the model cannot count tokens.
fn approx_words(tokens: u64) -> u64 {
    tokens.saturating_mul(3) / 4
}

/// The system prompt for a summary request: [`SUMMARY_SYSTEM_PROMPT`] plus the length ceiling
/// `check_summary` enforces, stated in tokens and in approximate English words (dogfood7: the
/// model wrote a 10k-token summary twice because nothing told it the limit).
pub fn summary_system_prompt(max_tokens: u64) -> String {
    let facts = verified_facts_max_tokens(max_tokens);
    format!(
        "{SUMMARY_SYSTEM_PROMPT}\n\nThe entire summary must be shorter than {max_tokens} tokens \
         (about {} English words; that is an approximation, 1 token ≈ 0.75 words), and the \
         Verified facts section alone must be at most {facts} tokens (about {} English words; \
         same approximation). When the summary is over the limit, drop stale details from the \
         other sections first, then follow the Verified facts deletion rule above. This applies \
         when updating a previous summary too: shorten it, do not only append.",
        approx_words(max_tokens),
        approx_words(facts)
    )
}

/// The line the compacted history's summary message starts with, so a reader (and the model)
/// can tell a summary from an ordinary user message.
pub const SUMMARY_MESSAGE_PREFIX: &str = "\
This is a summary of the earlier conversation. Continue the work from it.";

/// The heading that separates the model's summary from the verbatim user messages in the
/// summary message's body (ADR 0018). The model's summary never contains it: it is
/// the split point [`compaction_summary_text`] uses, and the user messages are read back from
/// `origin`, never parsed out of the body.
pub const USER_MESSAGES_HEADING: &str = "\n\n---\nUser messages kept verbatim (oldest first):";

/// The summary length ceiling for an input budget `B` (dogfood9):
/// `min(K, max(8192, ⌊B/8⌋))` with `K = keep_target(B)`. The old fixed [`DEFAULT_SUMMARY_MAX_TOKENS`]
/// was too small for a large window: the rolling summary grows with every compaction, and at
/// 120K (`B = 103616`) the shortening retry still produced 8725 tokens, over 8192, and failed
/// the whole turn. The ceiling now grows with the window — 12952 there — while never exceeding
/// the keep target. A smaller window keeps `K` (`B = 16000` → 4000). When there is **no
/// budget**, the caller uses [`DEFAULT_SUMMARY_MAX_TOKENS`] (8192) directly, because there is
/// no window to scale against.
pub fn summary_max_tokens(input_budget: u64) -> u64 {
    keep_target(input_budget).min(DEFAULT_SUMMARY_MAX_TOKENS.max(input_budget / 8))
}

/// ADR 0018's keep target `K = min(20000, max(2048, ⌊input_budget/4⌋))`. A target, not a hard
/// limit: a single atomic unit larger than `K` is still kept in full.
pub fn keep_target(input_budget: u64) -> u64 {
    (input_budget / 4).clamp(MIN_KEEP_TARGET, MAX_KEEP_TARGET)
}

/// The default per-message token estimate: the conservative [`budget`] ruler applied to the
/// message's own JSON. It errs high on non-ASCII text, which is the safe direction for a
/// keep target.
pub fn message_tokens(message: &Message) -> u64 {
    let request = ModelRequest {
        system_prompt: None,
        messages: vec![message.clone()],
        tools: Vec::new(),
        settings: Value::Null,
    };
    budget::estimate_tokens(&request, DEFAULT_BYTES_PER_TOKEN)
}

/// Splits `history` into atomic units, each covering a contiguous range.
///
/// A [`Message::User`] is its own unit. An assistant message and every tool result that
/// answers one of its calls are one unit, so a cut can never leave a call without its result
/// or a result without its call. The ranges are contiguous and cover the whole history.
///
/// The history must pass [`validate_history`]; an invalid history is an error, never a guess
/// (cutting an invalid history would produce a compacted history that is invalid too).
pub fn units(history: &[Message]) -> Result<Vec<Range<usize>>, SessionError> {
    validate_history(history)?;
    let mut units = Vec::new();
    let mut index = 0;
    while index < history.len() {
        match &history[index] {
            Message::User { .. } => {
                units.push(index..index + 1);
                index += 1;
            }
            Message::Assistant { tool_calls, .. } => {
                let start = index;
                index += 1;
                // `validate_history` guarantees that the only messages that may follow before
                // the next user/assistant are the results of this assistant's calls.
                while let Some(Message::Tool { result, .. }) = history.get(index) {
                    if !tool_calls.iter().any(|call| call.id == result.call_id) {
                        return Err(SessionError::InvalidHistory(format!(
                            "message {index}: result for call {} does not answer message {start}",
                            result.call_id
                        )));
                    }
                    index += 1;
                }
                units.push(start..index);
            }
            Message::Tool { result, .. } => {
                // Unreachable after `validate_history`, kept as an explicit error rather than
                // an `unreachable!` so a future validator change cannot silently mis-unit.
                return Err(SessionError::InvalidHistory(format!(
                    "message {index}: result for call {} has no call",
                    result.call_id
                )));
            }
        }
    }
    Ok(units)
}

/// A compaction plan: which suffix to keep and which prefix to summarize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The message index the kept suffix starts at (always a unit boundary).
    pub first_kept: usize,
    /// The messages to summarize, `0..first_kept`.
    pub summarized: Range<usize>,
    /// The estimated tokens of the kept suffix.
    pub kept_tokens: u64,
    /// The estimated tokens of the summarized prefix.
    pub summarized_tokens: u64,
}

/// Why no plan could be made.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// The whole history already fits the keep target: there is nothing worth summarizing.
    #[error("nothing to compact: the history already fits the keep target")]
    NothingToCompact,
    /// The history is not valid, so it cannot be cut safely.
    #[error(transparent)]
    InvalidHistory(#[from] SessionError),
}

/// Plans a compaction of `history` against `input_budget`.
///
/// `sizer` estimates one message in tokens; [`message_tokens`] is the conservative default
/// and a test can inject a fixed size. The plan keeps the **smallest** unit boundary whose
/// suffix fits `K = keep_target(input_budget)`; when even the last unit alone is larger than
/// `K`, that unit is still kept whole (whether the result fits `B` is the caller's check,
/// per ADR 0018). `first_kept == 0` means the history already fits and there is nothing to
/// compact.
pub fn plan(
    history: &[Message],
    input_budget: u64,
    sizer: &dyn Fn(&Message) -> u64,
) -> Result<Plan, PlanError> {
    plan_with_keep(history, keep_target(input_budget), sizer)
}

/// [`plan`] with an explicit keep target `K` (ADR 0018's overflow fallback uses `K = 20000`
/// regardless of any budget). The splitting rules are identical, so the two callers cannot
/// diverge.
pub fn plan_with_keep(
    history: &[Message],
    keep: u64,
    sizer: &dyn Fn(&Message) -> u64,
) -> Result<Plan, PlanError> {
    let units = units(history)?;
    let Some((last, rest)) = units.split_last() else {
        // An empty history has no unit to keep and nothing to summarize.
        return Err(PlanError::NothingToCompact);
    };
    let target = keep;
    let size_of = |range: &Range<usize>| -> u64 { history[range.clone()].iter().map(sizer).sum() };

    // Walk the units backwards from the last one, which is kept unconditionally.
    let mut kept_tokens = size_of(last);
    let mut first_kept = units.len() - 1;
    for (i, unit) in rest.iter().enumerate().rev() {
        let next = kept_tokens.saturating_add(size_of(unit));
        if next > target {
            break;
        }
        kept_tokens = next;
        first_kept = i;
    }

    if first_kept == 0 {
        return Err(PlanError::NothingToCompact);
    }
    let first = units[first_kept].start;
    let summarized_tokens = size_of(&(0..first));
    Ok(Plan {
        first_kept: first,
        summarized: 0..first,
        kept_tokens,
        summarized_tokens,
    })
}

/// Why a summary was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SummaryError {
    #[error("the summary is empty")]
    Empty,
    #[error("the summary is {tokens} tokens, over the limit of {max}")]
    TooLong { tokens: u64, max: u64 },
}

/// Validates a summary: non-empty after trimming, and within `max_tokens` by the same
/// conservative ruler the keep target uses. Returns the trimmed summary on success.
pub fn check_summary(text: &str, max_tokens: u64) -> Result<String, SummaryError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(SummaryError::Empty);
    }
    let tokens = message_tokens(&Message::user(trimmed));
    if tokens > max_tokens {
        return Err(SummaryError::TooLong {
            tokens,
            max: max_tokens,
        });
    }
    Ok(trimmed.to_owned())
}

/// Builds the model request that asks for a summary of `to_summarize`.
///
/// The messages are flattened into **plain text** in one user message: the summarizer never
/// sees the raw tool-call/result shapes, so it cannot produce a broken pairing. Each tool
/// result, and each call's JSON arguments, is cut to [`TOOL_RESULT_LIMIT_CHARS`] characters
/// (never mid-character) and the cut is stated. An assistant's `reasoning` is left out: it is provider-invisible and not part
/// of the visible history (ADR 0014). `previous_summary` is the rolling summary of an earlier
/// compaction; `instructions` is the optional `/compact` note. No tools are sent.
pub fn summary_request(
    previous_summary: Option<&str>,
    to_summarize: &[Message],
    instructions: Option<&str>,
    max_tokens: u64,
    settings: Value,
) -> ModelRequest {
    let mut body = String::new();
    if let Some(previous) = previous_summary.map(str::trim).filter(|p| !p.is_empty()) {
        body.push_str("<previous-summary>\n");
        body.push_str(previous);
        body.push_str("\n</previous-summary>\n\n");
        body.push_str(
            "The messages below are NEW conversation messages. Update the previous summary \
             above with them, preserving what is still relevant.\n\n",
        );
    }
    body.push_str("Conversation to summarize:\n\n");
    body.push_str(&summarize_text(to_summarize));
    if let Some(instructions) = instructions.map(str::trim).filter(|i| !i.is_empty()) {
        body.push_str("\nAdditional instructions for the summary:\n");
        body.push_str(instructions);
    }
    ModelRequest {
        system_prompt: Some(summary_system_prompt(max_tokens)),
        messages: vec![Message::user(body)],
        tools: Vec::new(),
        settings,
    }
}

/// Builds the **shortening** retry request: when the first summary came back over the limit,
/// the second call sends only that over-long summary and asks for a shorter one that keeps the
/// seven sections. The original conversation is deliberately **not** resent, so the model is
/// not tempted to write another 10k-token roll-up (dogfood7). `target_tokens` is
/// [`shorten_target`], below the enforced limit to leave margin.
///
/// `settings` is the host's request-level compaction setting (ADR 0018), the same
/// value the first summary request carried: the retry is the same kind of call and must run at
/// the same reasoning level.
pub fn shorten_request(
    overlong_summary: &str,
    target_tokens: u64,
    settings: Value,
) -> ModelRequest {
    let facts = verified_facts_max_tokens(target_tokens);
    let body = format!(
        "The summary below is over the length limit. Rewrite it shorter, keeping the same sections. \
         When the summary is over the limit, first drop stale details from the other sections; \
         delete a fact only when it is no longer relevant to the Goal and to the Pending or In \
         progress work, and then delete the oldest fact first (list the facts in chronological \
         order, oldest first, and append each new fact at the end). The whole summary must be \
         shorter than {target_tokens} tokens (about {} English words; that is an approximation, \
         1 token ≈ 0.75 words), and the Verified facts section alone must be at most {facts} \
         tokens (about {} English words). Do not add new content and do not only append: the \
         whole summary must fit.\n\n<summary-to-shorten>\n{overlong_summary}\n</summary-to-shorten>",
        approx_words(target_tokens),
        approx_words(facts)
    );
    ModelRequest {
        system_prompt: Some(summary_system_prompt(target_tokens)),
        messages: vec![Message::user(body)],
        tools: Vec::new(),
        settings,
    }
}

/// The compacted history: the summary message followed by `kept` unchanged.
///
/// The summary is a [`Message::User`] with `source = System` and an
/// `origin = {"kind":"compaction_summary","user_messages":[…]}` marker, so a host, an exporter
/// or a future compactor can recognize it without parsing the text. `user_messages` are the
/// user's words Core carries verbatim (ADR 0018): they are rendered into the body
/// for the model, and written into `origin` so [`compaction_user_messages`] reads them back
/// without parsing the body. The result must pass [`validate_history`]; a failure is returned,
/// never committed.
pub fn compacted_history(
    summary: &str,
    user_messages: &[RetainedUserMessage],
    kept: &[Message],
) -> Result<Vec<Message>, SessionError> {
    let mut body = format!("{SUMMARY_MESSAGE_PREFIX}\n\n{}", summary.trim());
    if !user_messages.is_empty() {
        body.push_str(USER_MESSAGES_HEADING);
        body.push('\n');
        for (index, message) in user_messages.iter().enumerate() {
            body.push_str(&format!(
                "--- user message {} ---\n{}\n",
                index + 1,
                message.text
            ));
        }
    }
    let origin = serde_json::json!({
        "kind": "compaction_summary",
        "user_messages": user_messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>(),
    });
    let mut history = Vec::with_capacity(kept.len() + 1);
    history.push(Message::User {
        content: vec![ContentPart::text(body)],
        meta: MessageMeta {
            id: None,
            source: Some(InputSource::System),
            origin: Some(
                OpaqueJson::parse(origin.to_string()).expect("the origin we build is valid JSON"),
            ),
        },
    });
    history.extend(kept.iter().cloned());
    validate_history(&history)?;
    Ok(history)
}

/// Flattens messages into the text the summarizer reads.
fn summarize_text(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            Message::User { content, .. } => {
                out.push_str("User: ");
                out.push_str(&joined_text(content));
                out.push('\n');
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                // `reasoning` is deliberately not read (ADR 0014).
                out.push_str("Assistant: ");
                out.push_str(&joined_text(content));
                out.push('\n');
                for call in tool_calls {
                    // Arguments can be as large as a result (a whole file to write), so they
                    // are cut by the same rule.
                    let (arguments, truncated) =
                        truncate_chars(&call.arguments.to_string(), TOOL_RESULT_LIMIT_CHARS);
                    out.push_str(&format!(
                        "  called {} {} with {arguments}",
                        call.name, call.id
                    ));
                    if let Some(cut) = truncated {
                        out.push_str(&format!(" … [truncated {cut} characters]"));
                    }
                    out.push('\n');
                }
            }
            Message::Tool { result, .. } => {
                let (text, truncated) = truncate_chars(&result.text(), TOOL_RESULT_LIMIT_CHARS);
                let status = if result.is_error { "error" } else { "ok" };
                out.push_str(&format!("  result {} ({status}): {text}", result.call_id));
                if let Some(cut) = truncated {
                    out.push_str(&format!(" … [truncated {cut} characters]"));
                }
                out.push('\n');
            }
        }
    }
    out
}

fn joined_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Cuts `text` to at most `limit` characters, returning the kept text and how many characters
/// were dropped. Cutting is by `char`, so a multi-byte character is never split.
fn truncate_chars(text: &str, limit: usize) -> (String, Option<usize>) {
    let total = text.chars().count();
    if total <= limit {
        return (text.to_owned(), None);
    }
    (text.chars().take(limit).collect(), Some(total - limit))
}

// ---------- ADR 0018, decision 3: the deterministic fallback ----------

/// The truncation marker appended to a kept tool result (ADR 0018, decision 3). It is
/// the same shape the summary input writes, so the same cut reads the same either way.
fn tool_result_truncation_marker(dropped: usize) -> String {
    format!(" … [truncated {dropped} characters]")
}

/// Cuts one tool result's text to at most [`TOOL_RESULT_LIMIT_CHARS`] characters, truncation
/// marker included, using [`truncate_chars`] — the same cut the summary input uses. `None` when
/// the result already fits.
///
/// The marker reserves its own room inside the limit, so the kept result never grows past
/// [`TOOL_RESULT_LIMIT_CHARS`] (the marker is useless without the count it names).
pub fn truncate_tool_result_text(text: &str) -> Option<String> {
    let total = text.chars().count();
    if total <= TOOL_RESULT_LIMIT_CHARS {
        return None;
    }
    // Reserve the longest marker this text could need, so no rounding of the count can push the
    // result back over the limit.
    let reserve = tool_result_truncation_marker(total).chars().count();
    let limit = TOOL_RESULT_LIMIT_CHARS.saturating_sub(reserve);
    let (kept, cut) = truncate_chars(text, limit);
    let cut = cut.expect("the text is over the limit");
    Some(format!("{kept}{}", tool_result_truncation_marker(cut)))
}

/// Truncates every tool result in `kept` that is over [`TOOL_RESULT_LIMIT_CHARS`] (ADR 0018
/// revision 8, decision 3), in place, and reports how many were cut. A cut result keeps only
/// its textual content: `structured` and non-text parts cannot be cut by character and are
/// dropped rather than sent whole.
pub fn truncate_kept_tool_results(kept: &mut [Message]) -> usize {
    let mut cut = 0;
    for message in kept.iter_mut() {
        let Message::Tool { result, .. } = message else {
            continue;
        };
        let Some(text) = truncate_tool_result_text(&result.text()) else {
            continue;
        };
        result.content = vec![ContentPart::text(text)];
        result.structured = None;
        cut += 1;
    }
    cut
}

/// The fixed line a deterministic summary carries (ADR 0018, decision 3): what the
/// fallback dropped and how many kept tool results it cut. It is not a model summary, and a
/// reader can tell the two apart by it.
pub fn deterministic_summary_line(dropped: usize, truncated: usize) -> String {
    format!(
        "[context compacted without a summary: {dropped} messages dropped, \
         {truncated} tool results truncated]"
    )
}

/// The deterministic fallback's summary text (ADR 0018, decision 3): the previous
/// model summary **verbatim** (when there is one) followed by [`deterministic_summary_line`].
/// The previous summary is kept because the fallback must not throw away what an earlier model
/// summary established. The verbatim user messages are rendered by [`compacted_history`], not
/// here.
pub fn deterministic_summary(
    previous_summary: Option<&str>,
    dropped: usize,
    truncated: usize,
) -> String {
    let line = deterministic_summary_line(dropped, truncated);
    match previous_summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(previous) => format!("{previous}\n\n{line}"),
        None => line,
    }
}

/// Whether `message` is a compaction summary: a user message whose `origin.kind` is
/// `"compaction_summary"` (ADR 0018). A host, exporter or reader uses this instead of parsing
/// the text.
pub fn is_compaction_summary(message: &Message) -> bool {
    is_compaction_summary_origin(message)
}

fn is_compaction_summary_origin(message: &Message) -> bool {
    let Message::User { meta, .. } = message else {
        return false;
    };
    let Some(origin) = meta.origin.as_ref() else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(origin.get()) else {
        return false;
    };
    value.get("kind").and_then(Value::as_str) == Some("compaction_summary")
}

/// The rolling **model** summary of a compaction-summary message: the fixed
/// [`SUMMARY_MESSAGE_PREFIX`] removed, and everything from [`USER_MESSAGES_HEADING`] on cut
/// off (the verbatim user messages are not part of the model's summary). `None` when `message`
/// is not one.
pub fn compaction_summary_text(message: &Message) -> Option<String> {
    if !is_compaction_summary_origin(message) {
        return None;
    }
    let Message::User { content, .. } = message else {
        return None;
    };
    let text = joined_text(content);
    let rest = text.strip_prefix(SUMMARY_MESSAGE_PREFIX)?;
    let summary = match rest.find(USER_MESSAGES_HEADING) {
        Some(index) => &rest[..index],
        None => rest,
    };
    Some(summary.trim().to_owned())
}

/// The user messages a compaction-summary message carries verbatim (ADR 0018),
/// read from `origin` — never parsed out of the body. An old summary message (or any message
/// that is not one) has no such field, so it reads back as an empty list; nothing is migrated.
pub fn compaction_user_messages(message: &Message) -> Vec<RetainedUserMessage> {
    let Message::User { meta, .. } = message else {
        return Vec::new();
    };
    let Some(origin) = meta.origin.as_ref() else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(origin.get()) else {
        return Vec::new();
    };
    if value.get("kind").and_then(Value::as_str) != Some("compaction_summary") {
        return Vec::new();
    }
    value
        .get("user_messages")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(|text| RetainedUserMessage {
                    text: text.to_owned(),
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------- ADR 0018: the user's words, kept verbatim ----------

/// The lower bound of the verbatim budget `U` (ADR 0018): it guarantees the newest
/// short user message fits even in a small window.
pub const MIN_USER_MESSAGE_BUDGET: u64 = 512;

/// The upper bound of `U`, and the value used when there is no window at all.
pub const MAX_USER_MESSAGE_BUDGET: u64 = 8_192;

/// The cap on the **first** user message `U₁` (ADR 0018): the task itself, usually a
/// plan, may be longer than one ordinary message. It is `min(2000, U)`, so it shrinks with a
/// small window instead of pushing the compacted request to 83% of `B` on every compaction.
pub const FIRST_USER_MESSAGE_MAX: u64 = 2_000;

/// The two-layer verbatim budget for an input budget `B`: `(U, U₁)`.
///
/// `U = max(512, min(8192, ⌊B/16⌋))` governs the user messages after the first; `U₁ =
/// min(2000, U)` governs the first. No window (`None`) takes `U = 8192`, `U₁ = 2000`, the same
/// way the summary ceiling falls back to a fixed value.
pub fn user_message_budget(input_budget: Option<u64>) -> (u64, u64) {
    let u = match input_budget {
        Some(budget) => (budget / 16).clamp(MIN_USER_MESSAGE_BUDGET, MAX_USER_MESSAGE_BUDGET),
        None => MAX_USER_MESSAGE_BUDGET,
    };
    (u, u.min(FIRST_USER_MESSAGE_MAX))
}

/// One user message Core carries verbatim across a compaction: its text, with every attached
/// file replaced by a `[file: P, N lines]` placeholder. It is also the shape written into the
/// summary message's `origin`, so the list reads back exactly.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetainedUserMessage {
    pub text: String,
}

/// The line that names the user messages a budget dropped (ADR 0018). It rides in
/// the retained list so the model sees that something is missing; [`select_user_messages`]
/// recognizes it and **carries its count forward** on the next rolling compaction instead of
/// accumulating one marker per round.
pub fn omitted_marker(count: usize) -> String {
    format!("[{count} earlier user messages omitted]")
}

/// The count a previous omission marker named, when `text` is one. The format is Core's own
/// ([`omitted_marker`]), so reading it back is a parse, not a guess.
fn omitted_marker_count(text: &str) -> Option<usize> {
    let rest = text.strip_prefix('[')?;
    let count = rest.strip_suffix(" earlier user messages omitted]")?;
    count.parse().ok()
}

/// Whether `text` is an omission marker rather than a real user message.
fn is_omitted_marker(text: &str) -> bool {
    omitted_marker_count(text).is_some()
}

/// Whether `message` is a user message Core keeps verbatim (ADR 0018): a
/// [`Message::User`] whose `source` is [`InputSource::User`] **or missing**. A missing source
/// counts as `User` at this summary layer only (the same rule the TUI uses to decide "did a
/// human say this"); it is not an equality of semantics, and ADR 0005's "`None` never equals
/// `User`" stands. Everything else — `System` (so the summary message itself), `SlashCommand`,
/// `Scheduler`, `Mcp`, `Subagent`, `PermissionDecision` — is not retained.
fn retained_text(message: &Message) -> Option<String> {
    let Message::User { content, meta } = message else {
        return None;
    };
    match meta.source {
        None | Some(InputSource::User) => {}
        Some(_) => return None,
    }
    let mut out = String::new();
    for part in content {
        if let ContentPart::Text { text, .. } = part {
            let rendered = match part.attachment_path() {
                Some(path) => format!("[file: {path}, {} lines]", text.lines().count()),
                None => text.clone(),
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&rendered);
        }
    }
    Some(out)
}

/// The tokens `text` costs by the same conservative ruler [`message_tokens`] uses.
fn text_tokens(text: &str) -> u64 {
    message_tokens(&Message::user(text))
}

/// Truncates `text` to at most `budget` tokens, keeping the first ~3/4 and the last ~1/4 with
/// an omission marker between them. `None` when it already fits.
///
/// The cut shrinks geometrically from the proportional one, so a very long message converges
/// in a few dozen measurements; the shape is what the ADR asks for ("about 3/4 and about
/// 1/4"), and the result always fits.
fn truncate_text(text: &str, budget: u64) -> Option<String> {
    let total = text_tokens(text);
    if total <= budget {
        return None;
    }
    let chars: Vec<char> = text.chars().collect();
    let build = |keep: usize| -> String {
        let head = keep * 3 / 4;
        let tail = keep - head;
        let head_text: String = chars[..head].iter().collect();
        let tail_text: String = chars[chars.len() - tail..].iter().collect();
        let body = format!("{head_text}{tail_text}");
        let omitted = total.saturating_sub(text_tokens(&body));
        format!("{head_text}[… {omitted} tokens omitted …]{tail_text}")
    };
    let mut keep = chars.len();
    loop {
        let candidate = build(keep);
        if text_tokens(&candidate) <= budget {
            return Some(candidate);
        }
        if keep == 0 {
            return Some(format!("[… {total} tokens omitted …]"));
        }
        keep = (keep * 9) / 10;
    }
}

/// The user messages Core carries verbatim through this compaction (ADR 0018).
///
/// `previous` is the list the earlier compaction wrote; `newly_summarized` are the messages
/// this compaction is about to summarize. The result is `previous + the new user messages`,
/// re-budgeted: the **first** message is always kept (cut to `U₁` when it is over), then the
/// rest fill `U` from the newest backwards, with `[N earlier user messages omitted]` naming
/// the ones that did not fit. The output is in chronological order.
///
/// `newly_summarized` is scanned for user messages only ([`retained_text`]); an attached file
/// becomes a `[file: P, N lines]` placeholder, never its body.
pub fn select_user_messages(
    previous: &[RetainedUserMessage],
    newly_summarized: &[Message],
    input_budget: Option<u64>,
) -> Vec<RetainedUserMessage> {
    select_user_messages_tracked(previous, newly_summarized, input_budget).0
}

/// [`select_user_messages`] plus how many of `newly_summarized`'s user messages the result
/// carries verbatim.
///
/// The deterministic fallback (ADR 0018, decision 3) needs the count to report `N`
/// in its fixed line: a user message the list carries is still in the new view, so only the
/// other summarized messages count as dropped. The second element is that many **carried**
/// messages out of `newly_summarized`, in the same selection the first element encodes, so the
/// two cannot drift.
pub fn select_user_messages_tracked(
    previous: &[RetainedUserMessage],
    newly_summarized: &[Message],
    input_budget: Option<u64>,
) -> (Vec<RetainedUserMessage>, usize) {
    let mut texts: Vec<String> = previous
        .iter()
        .filter(|kept| !is_omitted_marker(&kept.text))
        .map(|kept| kept.text.clone())
        .collect();
    // Everything from this index on in `texts` comes from `newly_summarized`; the entries before
    // it are the earlier compaction's own. `carried` counts only the former.
    let base = texts.len();
    // A previous marker's count is carried forward: those messages are still omitted, and the
    // new marker must name all of them, not just this round's.
    let mut omitted: usize = previous
        .iter()
        .filter_map(|kept| omitted_marker_count(&kept.text))
        .sum();
    for message in newly_summarized {
        if let Some(text) = retained_text(message) {
            texts.push(text);
        }
    }
    if texts.is_empty() {
        return (Vec::new(), 0);
    }
    let (u, u1) = user_message_budget(input_budget);

    // The first message is the task itself: always kept, cut to `U₁`. When it came from
    // `newly_summarized` (no earlier list), it counts as carried.
    let first = truncate_text(&texts[0], u1).unwrap_or_else(|| texts[0].clone());
    let mut carried = usize::from(base == 0);
    let mut kept: Vec<String> = Vec::new();
    let mut used = 0u64;
    // From the newest backwards: the newest is always placed, truncated into `U` when it alone
    // is larger. The walk **stops** at the first message that does not fit, so what is kept is
    // the contiguous newest run and everything older is omitted (ADR 0018)
    // — never a newer message plus an older short one with a gap between them.
    for (index, text) in texts[1..].iter().enumerate().rev() {
        let tokens = text_tokens(text);
        let entry = if tokens > u {
            // Only the newest may exceed `U`; it is truncated into `U`.
            if index + 1 == texts.len() - 1 {
                truncate_text(text, u).unwrap_or_else(|| text.clone())
            } else {
                omitted += index + 1;
                break;
            }
        } else {
            text.clone()
        };
        let entry_tokens = text_tokens(&entry);
        if used + entry_tokens > u {
            omitted += index + 1;
            break;
        }
        used += entry_tokens;
        if index + 1 >= base {
            carried += 1;
        }
        kept.push(entry);
    }
    kept.reverse();

    let mut out = Vec::with_capacity(kept.len() + 2);
    out.push(RetainedUserMessage { text: first });
    if omitted > 0 {
        out.push(RetainedUserMessage {
            text: omitted_marker(omitted),
        });
    }
    out.extend(kept.into_iter().map(|text| RetainedUserMessage { text }));
    (out, carried)
}
