//! The storage errors. The record store names a file and, for a broken file, its 1-based line;
//! the JSONL paths the registry and the one-time migration still read name a line too (ADR
//! 0021 D3/D4).

use std::path::PathBuf;

use crate::model::ProjectId;

/// Why a memory operation failed.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("{path}: cannot open: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: already locked by another writer")]
    Locked { path: PathBuf },
    #[error("{path}: line {line}: JSON parse failure (column {column}): {message}")]
    Corrupt {
        path: PathBuf,
        line: u64,
        column: usize,
        message: String,
    },
    #[error(
        "{path}: line {line}: unknown event {event} is marked critical; refusing to open the log"
    )]
    UnknownCritical {
        path: PathBuf,
        line: u64,
        event: String,
    },
    #[error("{path}: line {line}: expected seq {expected}, found {found}")]
    BadSeq {
        path: PathBuf,
        line: u64,
        expected: u64,
        found: u64,
    },
    #[error("{path}: line {line}: {message}")]
    Envelope {
        path: PathBuf,
        line: u64,
        message: String,
    },
    #[error("no project {0}")]
    UnknownProject(ProjectId),
    #[error("invalid memory record name {name:?}: {reason}")]
    InvalidName { name: String, reason: String },
    #[error("invalid memory record: {message}")]
    InvalidRecord { message: String },
    #[error("{path}: line {line}: {message}")]
    Frontmatter {
        path: PathBuf,
        line: usize,
        message: String,
    },
    #[doc(hidden)]
    #[error("simulated crash (test seam)")]
    SimulatedCrash,
}

impl MemoryError {
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        MemoryError::Open {
            path: path.to_path_buf(),
            source,
        }
    }
}
