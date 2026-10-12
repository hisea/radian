//! Session JSONL v1 storage (ADR 0006): the durable journal seam for `agent-core`.
//!
//! This crate implements the session side of the JSONL format the ADR pins down:
//!
//! - [`SessionJournal`] is a [`agent_core::TranscriptJournal`]: every record the core
//!   commits is written to one `session_id` file — seven-field envelope, strict `seq` from 0,
//!   LF-terminated lines, and an fsync before `append` returns. One file allows exactly one
//!   writer (kernel `flock`), and opening an existing file for append first truncates any
//!   torn tail (ADR 0006 §3.2).
//! - [`Session`] is the streaming reader implementing the unknown-content policy of the ADR
//!   (§7): unknown optional fields are ignored, unknown non-critical events are skipped and
//!   reported, unknown critical events refuse the whole file, higher `schema_version` files
//!   refuse to be written. [`Session::history`] folds the journal into a transcript using
//!   `agent_core`'s own `replay`/`repair`/`recover` — never a reimplementation.
//!
//! There is deliberately no SQLite, no export, no session list in this slice (Phase 1 / A):
//! the JSONL file is the single source of truth and everything else is rebuilt from it.

mod error;
mod reader;
mod writer;

pub use error::ReadError;
pub use reader::{EventData, Session, SessionEvent};
pub use writer::{SessionCreatedData, SessionJournal, WriteError};
