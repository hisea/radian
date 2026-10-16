//! The journal seam: a host-provided, awaitable write-ahead record of transcript changes.
//!
//! The core knows nothing about files or formats here. It only promises that every change to
//! a session's in-memory transcript is offered to a [`TranscriptJournal`] and lands there
//! successfully before it lands in memory (see the ADR in `docs/adr/`).

use futures_util::future::BoxFuture;

use serde_json::Value;

use std::collections::{BTreeMap, BTreeSet};

use crate::message::{Message, MessageMeta, ToolResult};
use crate::model::ModelUsage;

/// One durable record. A1 journals transcript messages; A2 adds tool execution markers so
/// a crash between "dispatched" and "result written" can be classified on recovery.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum JournalRecord {
    /// A message appended to the transcript at `index`, which equals the transcript length
    /// at the time of the append.
    Message { index: usize, message: Message },
    /// Replaces the whole transcript. The following `Message` records rebuild the replacement
    /// from index zero; the record deliberately does not duplicate the history itself. The
    /// replacement only takes effect when a matching `TranscriptReplacementCommitted`
    /// follows (ADR 0019); until then the previous history stands.
    TranscriptReplaced { revision: u64, len: usize },
    /// The replacement opened by the matching `TranscriptReplaced` is complete and committed
    /// (ADR 0019). Only after this record does the replacement replace the history.
    TranscriptReplacementCommitted { revision: u64, len: usize },
    /// The replacement opened by the matching `TranscriptReplaced` was cut short and is
    /// discarded; the history stays the one before it (ADR 0019). Written by the writer's
    /// `resume` when a file ends inside a pending segment.
    TranscriptReplacementAborted { revision: u64 },
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
    /// Usage the provider reported for the assistant message that precedes this record (J5).
    /// Written only when the response carried usage; failure to append fails the turn like any
    /// other write (J6).
    ModelUsage { usage: ModelUsage },
    /// The real usage of one summarization model call (ADR 0018). Written as soon as the call
    /// returns, whether or not the compaction it belongs to commits, so the session's usage
    /// total includes it. It is **not** a `ModelUsage`: it must never move the budget baseline.
    CompactionUsage {
        revision: u64,
        model: String,
        usage: ModelUsage,
    },
    /// The candidate record of one compaction (ADR 0018). It carries the new view it wants to
    /// install; the compaction only takes effect when the following `TranscriptReplaced`
    /// commits under the same `revision` (ADR 0019). A candidate with no commit is ignored.
    ContextCompacted {
        revision: u64,
        source_revision: u64,
        source_len: usize,
        summary: String,
        first_kept: usize,
        tokens_before: u64,
        tokens_after: u64,
        below_target: bool,
        model: String,
        /// ADR 0018, decision 3: `true` for the deterministic fallback (no model
        /// call, so no `CompactionUsage`). `false` for a model summary.
        deterministic: bool,
    },
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
#[non_exhaustive]
pub enum ReplayError {
    #[error("journal message records are not contiguous: expected index {expected}, found {found}")]
    NotContiguous { expected: usize, found: usize },
    /// A replacement segment was opened and the next record is not one ADR 0019 §2 allows.
    #[error("transcript replacement: {detail}")]
    Replacement { detail: String },
}

/// A replacement opened by `TranscriptReplaced` and not yet closed.
#[derive(Debug, Clone, PartialEq)]
struct PendingReplacement {
    revision: u64,
    declared_len: usize,
    messages: Vec<Message>,
}

/// How an uncommitted replacement segment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplacementEnd {
    /// A matching `TranscriptReplacementAborted` closed it.
    Aborted,
    /// The file ended while the segment was still open (a crash).
    EndOfFile,
}

/// A replacement segment that never committed. Reported as a read-side diagnostic (ADR 0019).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncommittedReplacement {
    pub revision: u64,
    pub declared_len: usize,
    pub written: usize,
    pub end: ReplacementEnd,
}

/// The transcript state folded from a journal under ADR 0019 §2 — the **one** replacement
/// state machine `replay`, `repair` and `radian-session`'s reader all share.
///
/// An open replacement holds the previous `history` untouched. The pending segment accepts
/// only messages indexed `0..len`, a matching `TranscriptReplacementCommitted` (which swaps it
/// in and clears the previous revision's tool markers), or a matching
/// `TranscriptReplacementAborted` (which drops it). Any other record is corruption.
#[derive(Debug, Clone, Default)]
pub struct TranscriptFold {
    history: Vec<Message>,
    pending: Option<PendingReplacement>,
    settled: BTreeMap<String, ToolResult>,
    started: BTreeSet<String>,
    declared_calls: BTreeSet<String>,
    uncommitted: Vec<UncommittedReplacement>,
}

impl TranscriptFold {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one record.
    pub fn push(&mut self, record: &JournalRecord) -> Result<(), ReplayError> {
        match record {
            JournalRecord::Message { index, message } => self.push_message(*index, message),
            JournalRecord::TranscriptReplaced { revision, len } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a second TranscriptReplaced appeared inside it"));
                }
                self.pending = Some(PendingReplacement {
                    revision: *revision,
                    declared_len: *len,
                    messages: Vec::new(),
                });
                Ok(())
            }
            JournalRecord::TranscriptReplacementCommitted { revision, len } => {
                self.commit(*revision, *len)
            }
            JournalRecord::TranscriptReplacementAborted { revision } => self.abort(*revision),
            JournalRecord::ToolExecutionStarted { call_id, .. } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a tool marker appeared inside it"));
                }
                self.started.insert(call_id.clone());
                Ok(())
            }
            JournalRecord::ToolExecutionSettled { call_id, result } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a tool marker appeared inside it"));
                }
                self.settled.insert(call_id.clone(), result.clone());
                Ok(())
            }
            JournalRecord::ModelUsage { .. } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a usage record appeared inside it"));
                }
                Ok(())
            }
            // ADR 0018: the compaction candidate and its usage belong to the revision they
            // describe and are written before the replacement segment opens. Inside a pending
            // segment they are as illegal as any other foreign record, and neither changes the
            // history by itself.
            JournalRecord::CompactionUsage { .. } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a compaction usage record appeared inside it"));
                }
                Ok(())
            }
            JournalRecord::ContextCompacted { .. } => {
                if self.pending.is_some() {
                    return Err(self.unexpected("a ContextCompacted record appeared inside it"));
                }
                Ok(())
            }
        }
    }

    fn unexpected(&self, what: &str) -> ReplayError {
        let pending = self
            .pending
            .as_ref()
            .expect("only called with a segment open");
        ReplayError::Replacement {
            detail: format!(
                "{what} (revision {}, {}/{} messages written)",
                pending.revision,
                pending.messages.len(),
                pending.declared_len
            ),
        }
    }

    fn push_message(&mut self, index: usize, message: &Message) -> Result<(), ReplayError> {
        if let Some(pending) = &mut self.pending {
            let expected = pending.messages.len();
            if index != expected {
                return Err(ReplayError::NotContiguous {
                    expected,
                    found: index,
                });
            }
            if pending.messages.len() >= pending.declared_len {
                return Err(ReplayError::Replacement {
                    detail: format!(
                        "more messages than the declared len {} (revision {})",
                        pending.declared_len, pending.revision
                    ),
                });
            }
            pending.messages.push(message.clone());
            return Ok(());
        }
        let expected = self.history.len();
        if index != expected {
            return Err(ReplayError::NotContiguous {
                expected,
                found: index,
            });
        }
        if let Message::Assistant { tool_calls, .. } = message {
            for call in tool_calls {
                self.declared_calls.insert(call.id.clone());
            }
        }
        self.history.push(message.clone());
        Ok(())
    }

    fn commit(&mut self, revision: u64, len: usize) -> Result<(), ReplayError> {
        let Some(pending) = self.pending.take() else {
            return Err(ReplayError::Replacement {
                detail: format!(
                    "a commit for revision {revision} (len {len}) arrived with no replacement open"
                ),
            });
        };
        if pending.revision != revision
            || pending.declared_len != len
            || pending.messages.len() != len
        {
            return Err(ReplayError::Replacement {
                detail: format!(
                    "a commit for revision {revision} (len {len}) does not match the open segment \
                     revision {} ({} written of {})",
                    pending.revision,
                    pending.messages.len(),
                    pending.declared_len
                ),
            });
        }
        self.history = pending.messages;
        self.settled.clear();
        self.started.clear();
        self.declared_calls.clear();
        for message in &self.history {
            if let Message::Assistant { tool_calls, .. } = message {
                for call in tool_calls {
                    self.declared_calls.insert(call.id.clone());
                }
            }
        }
        Ok(())
    }

    fn abort(&mut self, revision: u64) -> Result<(), ReplayError> {
        let Some(pending) = self.pending.take() else {
            return Err(ReplayError::Replacement {
                detail: format!(
                    "an abort for revision {revision} arrived with no replacement open"
                ),
            });
        };
        if pending.revision != revision {
            return Err(ReplayError::Replacement {
                detail: format!(
                    "an abort for revision {revision} does not match the open segment revision {}",
                    pending.revision
                ),
            });
        }
        self.uncommitted.push(UncommittedReplacement {
            revision,
            declared_len: pending.declared_len,
            written: pending.messages.len(),
            end: ReplacementEnd::Aborted,
        });
        Ok(())
    }

    /// Closes an open segment as uncommitted at end of file.
    pub fn finish(mut self) -> Self {
        if let Some(pending) = self.pending.take() {
            self.uncommitted.push(UncommittedReplacement {
                revision: pending.revision,
                declared_len: pending.declared_len,
                written: pending.messages.len(),
                end: ReplacementEnd::EndOfFile,
            });
        }
        self
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn into_history(self) -> Vec<Message> {
        self.history
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn uncommitted(&self) -> &[UncommittedReplacement] {
        &self.uncommitted
    }

    /// Whether an assistant message of the **current** revision declared `call_id` (the tool
    /// marker rule, ADR 0005/0006 §7). Reset when a replacement commits.
    pub fn is_call_declared(&self, call_id: &str) -> bool {
        self.declared_calls.contains(call_id)
    }

    /// The effective history with `repair` applied (ADR 0002): the committed messages plus a
    /// synthetic result for every unpaired call of the current revision. Read-only, so an
    /// incremental reader (ADR 0023 U6's `JournalTail`) can ask for the history without
    /// finishing the fold; it matches `recover`'s result for the records folded so far.
    pub fn recovered_history(&self) -> Vec<Message> {
        let mut history = self.history.clone();
        history.extend(repair_with(&self.history, &self.settled, &self.started));
        history
    }
}

fn repair_with(
    history: &[Message],
    settled: &BTreeMap<String, ToolResult>,
    started: &BTreeSet<String>,
) -> Vec<Message> {
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
        .map(|id| match settled.get(&id) {
            Some(result) => Message::Tool {
                result: result.clone(),
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

/// Rebuilds the transcript messages from the `Message` records, in order, under ADR 0019 §2.
/// Marker records are ignored; a committed replacement swaps the history in.
pub fn replay(records: &[JournalRecord]) -> Result<Vec<Message>, ReplayError> {
    let mut fold = TranscriptFold::new();
    for record in records {
        fold.push(record)?;
    }
    Ok(fold.finish().into_history())
}

/// The `Message::Tool` results to append so every unpaired call of `history` is answered, in
/// assistant call order:
///
/// 1. a `ToolExecutionSettled` record exists for the call: its final result;
/// 2. only a `ToolExecutionStarted` exists: `outcome_unknown_after_crash`;
/// 3. neither exists: `not_started_after_crash`.
///
/// The markers are those of the current revision: a committed replacement clears the previous
/// ones, an aborted one does not.
pub fn repair(history: &[Message], records: &[JournalRecord]) -> Vec<Message> {
    let mut fold = TranscriptFold::new();
    for record in records {
        // `recover` calls `replay` first, so a corrupt journal never reaches here; a direct
        // caller gets a best-effort fold rather than a panic.
        let _ = fold.push(record);
    }
    let fold = fold.finish();
    repair_with(history, &fold.settled, &fold.started)
}

/// Replays the records and completes the transcript with `repair`, yielding a history that
/// answers every tool call. An empty journal yields an empty history.
pub fn recover(records: &[JournalRecord]) -> Result<Vec<Message>, ReplayError> {
    let mut fold = TranscriptFold::new();
    for record in records {
        fold.push(record)?;
    }
    let fold = fold.finish();
    let repaired = repair_with(fold.history(), &fold.settled, &fold.started);
    let mut history = fold.into_history();
    history.extend(repaired);
    Ok(history)
}
