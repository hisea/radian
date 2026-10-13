use std::path::PathBuf;

/// Why a session file could not be read. Every refusal names the path, and every
/// per-line diagnostic names the 1-based line number (ADR 0006 §7).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{path}: cannot open: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: line {line}: JSON parse failure (column {column}): {message}")]
    Corrupt {
        path: PathBuf,
        line: u64,
        column: usize,
        message: String,
    },
    #[error(
        "{path}: line {line}: unknown event {event} is marked critical and this reader \
         supports schema v1, so the whole file is refused"
    )]
    UnknownCritical {
        path: PathBuf,
        line: u64,
        event: String,
    },
    #[error(
        "{path}: schema_version {found} exceeds the supported v{supported}; the file may \
         only be opened read-only or refused, and must never be written"
    )]
    SchemaTooNew {
        path: PathBuf,
        found: u64,
        supported: u64,
    },
    #[error("{path}: unsupported schema_version {found} (expected 1)")]
    UnsupportedSchema { path: PathBuf, found: u64 },
    #[error("{path}: line {line}: expected seq {expected}, found {found}")]
    BadSeq {
        path: PathBuf,
        line: u64,
        expected: u64,
        found: u64,
    },
    #[error("{path}: line {line}: SessionCreated must be at seq 0")]
    SessionCreatedNotFirst { path: PathBuf, line: u64 },
    #[error("{path}: line {line}: {message}")]
    Envelope {
        path: PathBuf,
        line: u64,
        message: String,
    },
    #[error(
        "{path}: line {line}: MessageAppended index {found} is not the expected {expected} \
         (contiguous from 0, reset to 0 after a TranscriptReplaced)"
    )]
    BadIndex {
        path: PathBuf,
        line: u64,
        expected: u64,
        found: u64,
    },
    #[error(
        "{path}: line {line}: {event} for call_id {call_id:?} does not appear in an earlier \
         assistant message of the current transcript revision (ADR 0006 §7)"
    )]
    OrphanToolMarker {
        path: PathBuf,
        line: u64,
        event: String,
        call_id: String,
    },
    #[error(
        "{path}: line {line}: the previous TranscriptReplaced.len {len} is not satisfied \
         (only {count} message records follow it) and a new replacement begins"
    )]
    BadReplacementLen {
        path: PathBuf,
        line: u64,
        len: u64,
        count: u64,
    },
    #[error("replaying the journal: {0}")]
    Replay(#[from] agent_core::ReplayError),
}
