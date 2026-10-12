//! The journal seam: a host-provided, awaitable write-ahead record of transcript changes.
//!
//! The core knows nothing about files or formats here. It only promises that every change to
//! a session's in-memory transcript is offered to a [`TranscriptJournal`] and lands there
//! successfully before it lands in memory (see the ADR in `docs/adr/`).

use futures_util::future::BoxFuture;

use serde_json::Value;

use crate::message::{Message, MessageMeta, ToolResult};

/// One durable record. A1 journals transcript messages; A2 adds tool execution markers so
/// a crash between "dispatched" and "result written" can be classified on recovery.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum JournalRecord {
    /// A message appended to the transcript at `index`, which equals the transcript length
    /// at the time of the append.
    Message { index: usize, message: Message },
    /// Replaces the whole transcript. The following `Message` records rebuild the replacement
    /// from index zero; the record deliberately does not duplicate the history itself.
    TranscriptReplaced { revision: u64, len: usize },
    /// A tool call was handed to the tool session: the tool's own execution may begin.
    /// `arguments` are the final ones (after any `Rewrite`), so they are exactly what the
    /// tool saw.
    ToolExecutionStarted {
        call_id: String,
        name: String,
        arguments: Value,
    },
    /// A dispatched call settled: `result` is the final one (after `after_tool_call`),
    /// exactly what the transcript's result message will carry.
    ToolExecutionSettled { call_id: String, result: ToolResult },
}

/// Why a journal write was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct JournalError(pub String);

/// Receives every transcript modification before it is applied to memory.
///
/// The core awaits `append` and never cancels an append in flight (D3): cancellation and
/// deadlines converge only after the current append settles. Appends within one session are
/// strictly serialized (D4). If an append returns `Err`, the turn ends with
/// [`crate::RunError::Journal`] and the session needs recovery (D6).
pub trait TranscriptJournal: Send + Sync {
    /// Durably records `record`. The future must be awaited to completion; dropping it
    /// leaves the write's outcome unknown.
    fn append(&self, record: JournalRecord) -> BoxFuture<'static, Result<(), JournalError>>;
}

/// Why a journal cannot be replayed into a transcript.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    #[error("journal message records are not contiguous: expected index {expected}, found {found}")]
    NotContiguous { expected: usize, found: usize },
}

/// Rebuilds the transcript messages from the `Message` records, in order. Indexes must run
/// contiguously from 0; the first gap or repeat is reported. Marker records are ignored.
/// `TranscriptReplaced` starts a new history, so it clears the rebuilt messages and resets the
/// expected index to zero.
pub fn replay(records: &[JournalRecord]) -> Result<Vec<Message>, ReplayError> {
    let mut messages = Vec::new();
    for record in records {
        match record {
            JournalRecord::TranscriptReplaced { .. } => messages.clear(),
            JournalRecord::Message { index, message } => {
                let expected = messages.len();
                if *index != expected {
                    return Err(ReplayError::NotContiguous {
                        expected,
                        found: *index,
                    });
                }
                messages.push(message.clone());
            }
            JournalRecord::ToolExecutionStarted { .. }
            | JournalRecord::ToolExecutionSettled { .. } => {}
        }
    }
    Ok(messages)
}

/// The `Message::Tool` results to append so every unpaired call of `history` is answered,
/// in assistant call order:
///
/// 1. a `ToolExecutionSettled` record exists for the call: its final result;
/// 2. only a `ToolExecutionStarted` exists: `outcome_unknown_after_crash` — the tool may
///    already have run, the outcome must not be assumed;
/// 3. neither exists: `not_started_after_crash`.
///
/// Already paired calls are left alone; a complete history yields an empty `Vec`.
pub fn repair(history: &[Message], records: &[JournalRecord]) -> Vec<Message> {
    let mut settled = std::collections::HashMap::new();
    let mut started = std::collections::HashSet::new();
    for record in records {
        match record {
            JournalRecord::ToolExecutionSettled { call_id, result } => {
                settled.insert(call_id.clone(), result.clone());
            }
            JournalRecord::ToolExecutionStarted { call_id, .. } => {
                started.insert(call_id.clone());
            }
            // Markers describe the old revision and must not repair its replacement.
            JournalRecord::TranscriptReplaced { .. } => {
                settled.clear();
                started.clear();
            }
            JournalRecord::Message { .. } => {}
        }
    }
    // Calls with no result message yet, in the order the assistants declared them.
    let mut open: Vec<String> = Vec::new();
    let mut answered = std::collections::HashSet::new();
    for message in history {
        match message {
            Message::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    open.push(call.id.clone());
                }
            }
            Message::Tool { result, .. } => {
                answered.insert(result.call_id.clone());
            }
            Message::User { .. } => {}
        }
    }
    open.retain(|id| !answered.contains(id));
    open.into_iter()
        .map(|id| match settled.remove(&id) {
            Some(result) => Message::Tool {
                result,
                meta: MessageMeta::default(),
            },
            None => {
                let code = if started.contains(&id) {
                    ("outcome_unknown_after_crash", "The tool may already have executed before the interruption; its outcome is unknown. Do not assume it did not run.")
                } else {
                    ("not_started_after_crash", "This call had not started executing when the run was interrupted.")
                };
                Message::Tool {
                    result: ToolResult::error(&id, code.0, code.1),
                    meta: MessageMeta::default(),
                }
            }
        })
        .collect()
}

/// Replays the records and completes the transcript with `repair`, yielding a history that
/// answers every tool call. An empty journal yields an empty history.
pub fn recover(records: &[JournalRecord]) -> Result<Vec<Message>, ReplayError> {
    let history = replay(records)?;
    let mut history = history;
    history.extend(repair(&history, records));
    Ok(history)
}
