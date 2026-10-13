//! Automatic compaction: the **pure planning half** (ADR 0018).
//!
//! ADR 0018 replaces the old "stop and ask a human" budget guard with automatic compaction:
//! when the projected input crosses the effective input budget `B`, the conversation is
//! summarized and the request is retried once. This module is only the part that can be
//! decided without I/O: how much recent history to keep, which messages form one atomic
//! unit, what to ask the summarizer for, whether the answer is usable, and what the
//! compacted history looks like. The turn loop, the journal writes, the model call and the
//! retry are the `compact-core` slice; nothing here is `async` and nothing here touches a
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

/// The default summary length ceiling (ADR 0018 says the summary must be within a limit but
/// does not fix one; this slice fixes 8192 tokens, deliberately not `B`).
pub const DEFAULT_SUMMARY_MAX_TOKENS: u64 = 8_192;

/// How many characters of one tool result reach the summarizer (ADR 0018).
pub const TOOL_RESULT_LIMIT_CHARS: usize = 2_000;

/// The fixed system prompt for a summary request (ADR 0018's structure). It asks for the
/// seven sections by name and forbids inventing anything, because the summary becomes part
/// of the next request's context.
pub const SUMMARY_SYSTEM_PROMPT: &str = "\
You are compressing a coding conversation so another agent can continue the work. \
Produce one structured summary with exactly these sections, in this order:

## Goal
## Constraints
## Progress
## Key decisions
## Next steps
## Critical context
## Files read and modified

Keep every section concise. Preserve exact file paths, function names, commands, and error \
messages. Include only what the conversation contains; never invent or guess. Output only the \
summary, with no preamble.";

/// The line the compacted history's summary message starts with, so a reader (and the model)
/// can tell a summary from an ordinary user message.
pub const SUMMARY_MESSAGE_PREFIX: &str = "\
This is a summary of the earlier conversation. Continue the work from it.";

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
    let units = units(history)?;
    let Some((last, rest)) = units.split_last() else {
        // An empty history has no unit to keep and nothing to summarize.
        return Err(PlanError::NothingToCompact);
    };
    let target = keep_target(input_budget);
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
/// result is cut to [`TOOL_RESULT_LIMIT_CHARS`] characters (never mid-character) and the cut
/// is stated. An assistant's `reasoning` is left out: it is provider-invisible and not part
/// of the visible history (ADR 0014). `previous_summary` is the rolling summary of an earlier
/// compaction; `instructions` is the optional `/compact` note. No tools are sent.
pub fn summary_request(
    previous_summary: Option<&str>,
    to_summarize: &[Message],
    instructions: Option<&str>,
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
        system_prompt: Some(SUMMARY_SYSTEM_PROMPT.to_owned()),
        messages: vec![Message::user(body)],
        tools: Vec::new(),
        settings: Value::Null,
    }
}

/// The compacted history: the summary message followed by `kept` unchanged.
///
/// The summary is a [`Message::User`] with `source = System` and an
/// `origin = {"kind":"compaction_summary"}` marker, so a host, an exporter or a future
/// compactor can recognize it without parsing the text. The result must pass
/// [`validate_history`]; a failure is returned, never committed.
pub fn compacted_history(summary: &str, kept: &[Message]) -> Result<Vec<Message>, SessionError> {
    let mut history = Vec::with_capacity(kept.len() + 1);
    history.push(Message::User {
        content: vec![ContentPart::text(format!(
            "{SUMMARY_MESSAGE_PREFIX}\n\n{}",
            summary.trim()
        ))],
        meta: MessageMeta {
            id: None,
            source: Some(InputSource::System),
            origin: Some(
                OpaqueJson::parse(r#"{"kind":"compaction_summary"}"#)
                    .expect("the origin literal is valid JSON"),
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
                    out.push_str(&format!(
                        "  called {} {} with {}\n",
                        call.name, call.id, call.arguments
                    ));
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
