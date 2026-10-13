//! Session fork (ADR 0022): materialize a parent's effective history at a turn boundary into
//! a new child session, then record the lineage in both files.
//!
//! The fork point is a **turn boundary** of the parent: the seq of a `TurnFinished`, or 0
//! (the `SessionCreated` seq) when no turn has finished. Everything the parent showed up to
//! that seq — a committed replacement in effect, an uncommitted one dropped, unpaired calls
//! repaired (ADR 0002/0019) — becomes the child's own `MessageAppended` records from index 0.
//! The parent's original facts are untouched except for one appended `SessionForked`.
//!
//! `SessionCreatedData.parent` is the authority on the lineage; the parent's `SessionForked`
//! is its audit index. The child is written and fsynced **first**, so the parent never records
//! a child that does not exist (ADR 0022 §4).

use std::path::PathBuf;

use agent_core::{JournalError, JournalRecord, Message, ReplayError, TranscriptJournal, recover};
use serde_json::Value;

use crate::error::ReadError;
use crate::reader::{Session, SessionEvent};
use crate::runtime::RuntimeEvent;
use crate::writer::{ForkParent, SessionCreatedData, SessionJournal, WriteError};

/// Why a fork could not be made.
#[derive(Debug, thiserror::Error)]
pub enum ForkError {
    #[error("session {session_id:?}: cannot read the parent: {source}")]
    Read {
        session_id: String,
        source: ReadError,
    },
    #[error("session {session_id:?}: no event with seq {seq}")]
    UnknownSeq { session_id: String, seq: u64 },
    #[error("session {session_id:?}: seq {seq} is past the last event (seq {last_seq})")]
    BeyondEnd {
        session_id: String,
        seq: u64,
        last_seq: u64,
    },
    #[error("session {session_id:?}: seq {seq} is not a turn boundary: {detail}")]
    NotATurnBoundary {
        session_id: String,
        seq: u64,
        detail: String,
    },
    #[error("session {session_id:?}: folding the parent failed: {source}")]
    Replay {
        session_id: String,
        source: ReplayError,
    },
    #[error("cannot create the child {child_id:?}: {source}")]
    Child {
        child_id: String,
        source: WriteError,
    },
    #[error("cannot write the child {child_id:?}: {error}")]
    ChildJournal {
        child_id: String,
        error: JournalError,
    },
    #[error("cannot record the fork in the parent {parent_id:?}: {source}")]
    Parent {
        parent_id: String,
        source: WriteError,
    },
}

/// A validated fork point: the parent seq and the effective history materialized there.
#[derive(Debug, Clone, PartialEq)]
pub struct ForkPoint {
    pub seq: u64,
    pub history: Vec<Message>,
}

/// Everything a fork needs. `seq: None` means the parent's last turn boundary.
#[derive(Debug, Clone)]
pub struct ForkRequest {
    pub parent_path: PathBuf,
    pub parent_id: String,
    pub seq: Option<u64>,
    pub child_path: PathBuf,
    pub child_id: String,
    pub workspace: String,
    pub config_snapshot: Value,
}

/// What a fork produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkOutcome {
    pub child_id: String,
    pub seq: u64,
    pub messages: usize,
}

/// The parent's last turn boundary: the seq of its last `TurnFinished`, or 0 (the
/// `SessionCreated` seq) when no turn has finished. ADR 0022 §1: fork only at a boundary.
pub fn last_fork_point(parent: &Session) -> u64 {
    parent
        .events()
        .iter()
        .rev()
        .find(|event| event.event == "TurnFinished")
        .map_or(0, |event| event.seq)
}

/// The parent's effective history at the turn boundary `seq` (ADR 0022 §2).
///
/// `seq` must be a `TurnFinished` seq, or 0 for the session start (empty history). Anything
/// else is refused with the reason: a seq inside a running turn, inside a replacement segment
/// (committed or not), a seq that names no event, or a seq past the end of the file.
pub fn fork_point_history(parent: &Session, seq: u64) -> Result<Vec<Message>, ForkError> {
    let session_id = parent.session_id().to_owned();
    let events = parent.events();
    let last_seq = events.last().map_or(0, |event| event.seq);
    if seq > last_seq {
        return Err(ForkError::BeyondEnd {
            session_id,
            seq,
            last_seq,
        });
    }
    let Some(event) = events.iter().find(|event| event.seq == seq) else {
        return Err(ForkError::UnknownSeq { session_id, seq });
    };
    if event.event != "SessionCreated" && event.event != "TurnFinished" {
        return Err(ForkError::NotATurnBoundary {
            session_id,
            seq,
            detail: boundary_detail(events, seq, event),
        });
    }
    let records = parent.journal_records_through(seq);
    recover(&records).map_err(|source| ForkError::Replay { session_id, source })
}

/// The `seq` and history of a fork point, resolved from an optional seq.
pub fn resolve_fork_point(parent: &Session, seq: Option<u64>) -> Result<ForkPoint, ForkError> {
    let seq = seq.unwrap_or_else(|| last_fork_point(parent));
    let history = fork_point_history(parent, seq)?;
    Ok(ForkPoint { seq, history })
}

/// Forks a session from disk (the CLI path). The parent is locked for writing **before** the
/// child is created, so a parent another process is running fails fast with no orphan child;
/// the `SessionForked` record is still written last (ADR 0022 §4).
pub async fn fork(request: ForkRequest) -> Result<ForkOutcome, ForkError> {
    let parent = open_parent(&request.parent_path, &request.parent_id)?;
    let point = resolve_fork_point(&parent, request.seq)?;
    drop(parent);
    let parent_journal =
        SessionJournal::resume(&request.parent_path, &request.parent_id).map_err(|source| {
            ForkError::Parent {
                parent_id: request.parent_id.clone(),
                source,
            }
        })?;
    let outcome = write_child_and_record(&request, &point, &parent_journal).await?;
    drop(parent_journal);
    Ok(outcome)
}

/// Forks a session whose writer this process already holds (the TUI `/fork` path): the parent
/// is opened for reading, the child is written, and the `SessionForked` goes through the
/// existing journal, so the single-writer lock is not taken twice.
pub async fn fork_with_journal(
    request: ForkRequest,
    parent_journal: &SessionJournal,
) -> Result<ForkOutcome, ForkError> {
    let parent = open_parent(parent_journal.path(), &request.parent_id)?;
    let point = resolve_fork_point(&parent, request.seq)?;
    drop(parent);
    write_child_and_record(&request, &point, parent_journal).await
}

fn open_parent(path: &std::path::Path, session_id: &str) -> Result<Session, ForkError> {
    Session::open(path).map_err(|source| ForkError::Read {
        session_id: session_id.to_owned(),
        source,
    })
}

/// Writes the child first (the caller has the parent journal ready), then appends the
/// parent's `SessionForked`. Any child failure returns before the parent is touched.
async fn write_child_and_record(
    request: &ForkRequest,
    point: &ForkPoint,
    parent_journal: &SessionJournal,
) -> Result<ForkOutcome, ForkError> {
    let created = SessionCreatedData::forked(
        request.workspace.clone(),
        request.config_snapshot.clone(),
        ForkParent {
            session_id: request.parent_id.clone(),
            seq: point.seq,
        },
    );
    let child = SessionJournal::create(&request.child_path, &request.child_id, created).map_err(
        |source| ForkError::Child {
            child_id: request.child_id.clone(),
            source,
        },
    )?;
    for (index, message) in point.history.iter().cloned().enumerate() {
        child
            .append(JournalRecord::Message { index, message })
            .await
            .map_err(|error| ForkError::ChildJournal {
                child_id: request.child_id.clone(),
                error,
            })?;
    }
    // Close the child (every append already fsynced) before the parent records it.
    drop(child);
    parent_journal
        .append_runtime_event(RuntimeEvent::SessionForked {
            child_session_id: request.child_id.clone(),
            parent_seq: point.seq,
        })
        .await
        .map_err(|source| ForkError::Parent {
            parent_id: parent_journal.session_id().to_owned(),
            source,
        })?;
    Ok(ForkOutcome {
        child_id: request.child_id.clone(),
        seq: point.seq,
        messages: point.history.len(),
    })
}

/// Why the event at `seq` is not a turn boundary, tracking the parent's state up to and
/// including that seq.
fn boundary_detail(events: &[SessionEvent], seq: u64, event: &SessionEvent) -> String {
    let mut in_segment = false;
    let mut in_turn = false;
    for event in events.iter().filter(|event| event.seq <= seq) {
        match event.event.as_str() {
            "TranscriptReplaced" => in_segment = true,
            "TranscriptReplacementCommitted" | "TranscriptReplacementAborted" => in_segment = false,
            "TurnStarted" => in_turn = true,
            "TurnFinished" => in_turn = false,
            _ => {}
        }
    }
    if in_segment {
        "it is inside a transcript replacement segment, not after a TurnFinished".to_owned()
    } else if in_turn {
        "the turn is still running".to_owned()
    } else {
        format!(
            "{} is not a turn boundary; fork after a TurnFinished (or at seq 0)",
            event.event
        )
    }
}
