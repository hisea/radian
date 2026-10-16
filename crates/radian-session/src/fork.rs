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

use std::path::{Path, PathBuf};

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
///
/// The child is built at a temp path in the same directory (ADR 0022 §4, review fix): the
/// session index lists only `*.jsonl`, so the `.jsonl.tmp` name is never seen as a session. On
/// success the complete, fsynced file is renamed into place and the directory is fsynced. On
/// any failure the temp file is removed and the child path never exists — a half-written child
/// can no longer look resumable while the parent has no `SessionForked`.
async fn write_child_and_record(
    request: &ForkRequest,
    point: &ForkPoint,
    parent_journal: &SessionJournal,
) -> Result<ForkOutcome, ForkError> {
    let created = SessionCreatedData::forked(
        request.workspace.clone(),
        request.config_snapshot.clone(),
        ForkParent::fork(request.parent_id.clone(), point.seq),
    );
    let temp_path = child_temp_path(&request.child_path);
    // A temp file left by an earlier crash must not block this attempt.
    let _ = std::fs::remove_file(&temp_path);
    let child = match SessionJournal::create(&temp_path, &request.child_id, created) {
        Ok(child) => child,
        Err(source) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(ForkError::Child {
                child_id: request.child_id.clone(),
                source,
            });
        }
    };
    let mut appended = 0usize;
    for (index, message) in point.history.iter().cloned().enumerate() {
        #[cfg(feature = "testing")]
        if take_child_failure(appended) {
            drop(child);
            return Err(child_failure(
                request,
                &temp_path,
                JournalError(format!("test seam: failing after {appended} messages")),
            ));
        }
        if let Err(error) = child
            .append(JournalRecord::Message { index, message })
            .await
        {
            drop(child);
            return Err(child_failure(request, &temp_path, error));
        }
        appended += 1;
    }
    // A failure between the last append and the rename is possible too.
    #[cfg(feature = "testing")]
    if take_child_failure(appended) {
        drop(child);
        return Err(child_failure(
            request,
            &temp_path,
            JournalError(format!("test seam: failing after {appended} messages")),
        ));
    }
    // Close the child (every append already fsynced), then publish it atomically.
    drop(child);
    if let Err(source) = publish_child(&temp_path, &request.child_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(ForkError::Child {
            child_id: request.child_id.clone(),
            source,
        });
    }
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

/// The child's temp path: `.<file name>.tmp` next to it, e.g. `.abc.jsonl.tmp`. The session
/// index lists only `*.jsonl`, so the temp file is never mistaken for a session (ADR 0022 §4).
fn child_temp_path(child_path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(child_path.file_name().unwrap_or_default());
    name.push(".tmp");
    child_path.with_file_name(name)
}

/// Removes the temp file and builds the child-journal failure.
fn child_failure(request: &ForkRequest, temp_path: &Path, error: JournalError) -> ForkError {
    let _ = std::fs::remove_file(temp_path);
    ForkError::ChildJournal {
        child_id: request.child_id.clone(),
        error,
    }
}

/// Renames the complete temp file over the child path and fsyncs the directory, so the rename
/// survives a crash (ADR 0022 §4).
fn publish_child(temp_path: &Path, child_path: &Path) -> Result<(), WriteError> {
    std::fs::rename(temp_path, child_path).map_err(|source| WriteError::Io {
        path: child_path.to_path_buf(),
        source,
    })?;
    if let Some(dir) = child_path.parent()
        && let Ok(dir_file) = std::fs::File::open(dir)
    {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Test seam (feature `testing`): fail the child write after `k` messages. Thread-local and
/// crate-private, so one test's armed seam cannot leak into another's (each `#[tokio::test]`
/// runs on its own thread). `k` counts messages already appended, so `0` fails before any
/// append and `1` after the first.
#[cfg(all(test, feature = "testing"))]
pub(crate) fn fail_child_after(messages: usize) {
    FAIL_CHILD_AFTER.with(|slot| slot.set(Some(messages)));
}

#[cfg(feature = "testing")]
thread_local! {
    static FAIL_CHILD_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "testing")]
fn take_child_failure(index: usize) -> bool {
    FAIL_CHILD_AFTER.with(|slot| {
        if slot.get() == Some(index) {
            slot.set(None);
            true
        } else {
            false
        }
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

#[cfg(all(test, feature = "testing"))]
mod tests {
    use super::*;
    use crate::reader::Session;
    use crate::runtime::TurnOutcome;

    fn temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-fork-seam-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A parent with one complete turn (two messages) and the boundary seq.
    async fn parent_with_one_turn(dir: &Path, parent_id: &str) -> (PathBuf, u64) {
        let path = dir.join("parent.jsonl");
        let journal = SessionJournal::create(
            &path,
            parent_id,
            SessionCreatedData::new("/work/example", serde_json::json!({})),
        )
        .unwrap();
        journal
            .append_runtime_event(RuntimeEvent::TurnStarted {
                turn_id: "t-1".into(),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("hi"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::assistant("hello"),
            })
            .await
            .unwrap();
        let seq = journal.next_seq();
        journal
            .append_runtime_event(RuntimeEvent::TurnFinished {
                turn_id: "t-1".into(),
                outcome: TurnOutcome::Completed,
                reason: None,
                stop_reason: None,
            })
            .await
            .unwrap();
        drop(journal);
        (path, seq)
    }

    fn fork_request(
        dir: &Path,
        parent_path: PathBuf,
        seq: u64,
        child_id: &str,
    ) -> (ForkRequest, PathBuf) {
        let child_path = dir.join("child.jsonl");
        (
            ForkRequest {
                parent_path,
                parent_id: "parent".into(),
                seq: Some(seq),
                child_path: child_path.clone(),
                child_id: child_id.to_owned(),
                workspace: "/work/example".into(),
                config_snapshot: serde_json::json!({}),
            },
            child_path,
        )
    }

    /// ADR 0022 §4 (review fix): a failure after k messages leaves no child file, no temp file
    /// and no parent `SessionForked`. On the old code the child was written directly, so the
    /// file existed half-written.
    #[tokio::test]
    async fn a_failed_child_leaves_nothing_behind() {
        for k in 0..3 {
            let dir = temp_dir(&format!("fail-{k}"));
            let (parent_path, seq) = parent_with_one_turn(&dir, "parent").await;
            let (request, child_path) = fork_request(&dir, parent_path.clone(), seq, "child");
            fail_child_after(k);
            let error = fork(request).await.unwrap_err();
            assert!(matches!(error, ForkError::ChildJournal { .. }), "{error:?}");
            assert!(!child_path.exists(), "a half child must not exist (k={k})");
            let leftovers: Vec<String> = std::fs::read_dir(&dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "no temp file left (k={k}): {leftovers:?}"
            );
            let parent = Session::open(&parent_path).unwrap();
            assert!(
                !parent
                    .events()
                    .iter()
                    .any(|event| event.event == "SessionForked"),
                "the parent must not record a fork that failed (k={k})"
            );
        }
    }

    /// The success path: the child is complete at the final path and the parent records it.
    #[tokio::test]
    async fn a_successful_child_is_complete_and_recorded() {
        let dir = temp_dir("ok");
        let (parent_path, seq) = parent_with_one_turn(&dir, "parent").await;
        let (request, child_path) = fork_request(&dir, parent_path.clone(), seq, "child");
        let outcome = fork(request).await.unwrap();
        assert_eq!(outcome.messages, 2);
        let child = Session::open(&child_path).unwrap();
        assert_eq!(child.events().len(), 3, "SessionCreated + two messages");
        let parent = Session::open(&parent_path).unwrap();
        assert!(
            parent
                .events()
                .iter()
                .any(|event| event.event == "SessionForked"),
            "the parent records the fork"
        );
    }

    /// The session index lists only `*.jsonl`; a hand-made temp file is not a session.
    #[test]
    fn a_temp_file_is_not_listed_as_a_session() {
        let dir = temp_dir("listing");
        // A real session file.
        let real = dir.join("child.jsonl");
        let journal = SessionJournal::create(
            &real,
            "child",
            SessionCreatedData::new("/work/example", serde_json::json!({})),
        )
        .unwrap();
        drop(journal);
        // A temp file with the same bytes: the index must ignore it.
        std::fs::write(dir.join(".child.jsonl.tmp"), std::fs::read(&real).unwrap()).unwrap();
        let mut index = crate::Index::open(dir.join("index.sqlite")).unwrap();
        index.rebuild_dir(&dir).unwrap();
        let rows = index.list(None).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].session_id, "child");
    }
}
