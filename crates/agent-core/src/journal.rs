//! The journal seam: a host-provided, awaitable write-ahead record of transcript changes.
//!
//! The core knows nothing about files or formats here. It only promises that every change to
//! a session's in-memory transcript is offered to a [`TranscriptJournal`] and lands there
//! successfully before it lands in memory (see the ADR in `docs/adr/`).

use futures_util::future::BoxFuture;

use crate::message::Message;

/// One durable record. A1 journals transcript messages only; later slices add more kinds.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum JournalRecord {
    /// A message appended to the transcript at `index`, which equals the transcript length
    /// at the time of the append.
    Message { index: usize, message: Message },
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
