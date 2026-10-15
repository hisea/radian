//! The storage errors. `Corrupt`/`UnknownCritical`/`BadSeq` name the 1-based line, like
//! `radian-session` (ADR 0006 §3.2/§7).

use std::path::PathBuf;

use crate::model::{MemoryId, Scope, Source};

/// Why a memory operation failed.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("{path}: cannot open: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: already locked by another writer; a memory log allows exactly one writer")]
    Locked { path: PathBuf },
    #[error("{path}: the file was replaced repeatedly while opening; giving up")]
    Raced { path: PathBuf },
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
    #[error("this store was not opened for scope {0:?}")]
    ScopeNotOpen(Scope),
    #[error("no memory record {id}")]
    NotFound { id: MemoryId },
    #[error("no project {0}")]
    UnknownProject(crate::model::ProjectId),
    #[error("record {id} has an unknown {field} value; it cannot be changed by this version")]
    UnknownRecordField { id: MemoryId, field: &'static str },
    #[doc(hidden)]
    #[error("simulated crash (test seam)")]
    SimulatedCrash,
    #[error("key {key:?} is already held by {id} at revision {revision}")]
    KeyConflict {
        key: String,
        id: MemoryId,
        revision: u64,
    },
    #[error("record {id} is at revision {found}, but revision {expected} was expected")]
    RevisionConflict {
        id: MemoryId,
        expected: u64,
        found: u64,
    },
    #[error("a {actor:?} writer may not modify {id}, which is a {existing:?} record")]
    SourceProtected {
        id: MemoryId,
        actor: Source,
        existing: Source,
    },
    #[error("markdown is not in the export format: line {line}, column {column}: {message}")]
    Markdown {
        line: usize,
        column: usize,
        message: String,
    },
}

impl MemoryError {
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        MemoryError::Open {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Where a Purge was interrupted, for the crash-at-every-step test seam (ADR 0021 D3). A real
/// crash leaves the same on-disk state: the atomic rename means the path holds either the old
/// log or the new one, never a half-written file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeStep {
    /// The temporary file has been written but not fsynced.
    TempWritten,
    /// The temporary file has been fsynced.
    TempFsynced,
    /// The rename happened but the directory was not fsynced.
    Renamed,
    /// Fully done.
    DirFsynced,
}
