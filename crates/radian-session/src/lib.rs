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
//! - [`Index`] is the **disposable** SQLite cache over the files (ADR 0006 §10): every
//!   column is re-derivable from the JSONL, a torn tail is never indexed, and the JSONL is
//!   never written. [`export_json`]/[`export_markdown`] render a session with non-optional
//!   secret redaction and an opt-in for raw provider payloads (ADR 0013 T4).

mod error;
mod export;
mod fork;
mod fs;
mod gate;
mod index;
mod reader;
mod runtime;
mod writer;

/// Re-exported so a reader of [`UncommittedReplacement::end`] need not depend on `agent-core`
/// directly for the enum.
pub use agent_core::ReplacementEnd;
pub use error::ReadError;
pub use export::{
    ExportError, ExportOptions, export_json, export_json_to, export_markdown, export_markdown_to,
    redact_text, redact_value,
};
pub use fork::{
    ForkError, ForkOutcome, ForkPoint, ForkRequest, fork, fork_point_history, fork_with_journal,
    last_fork_point, resolve_fork_point,
};
pub use fs::{
    PRIVATE_DIR_MODE, PRIVATE_FILE_MODE, ensure_private_dir, ensure_private_file,
    open_private_append, warn_if_wide, wide_permissions_warning,
};
pub use index::{
    Audit, Index, IndexError, IndexedEvent, RebuildWarning, Reconcile, SearchHit, SessionSummary,
    read_line_at,
};
pub use reader::{EventData, Session, SessionEvent, UncommittedReplacement};
pub use runtime::{
    InboxLane, MemoryInjectedRecord, PermissionApprover, PermissionDecision, RuntimeEvent,
    TurnOutcome, TurnReason,
};
pub use writer::{ForkParent, SessionCreatedData, SessionJournal, WriteError, now_utc};

/// Fault-injection seams, compiled only with the `testing` feature. Off by default so a
/// production build cannot arm them (ADR 0019 §7's failed-abort-write path).
#[cfg(feature = "testing")]
pub use writer::{fail_next_aborted_write, fail_next_write};
