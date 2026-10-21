//! ADR 0043 slice ⓪A acceptance: a journal written before the orchestration was removed still
//! opens and still replays. The task events (`TaskSpawned`, `TaskFinished`) are `critical: false`,
//! so the reader skips them as **unknown non-critical** events (ADR 0006 §7(d)) and reports them
//! in `Session::skipped`; nothing in the transcript fold changes.
//!
//! The content is the pre-change `docs/schema/fixtures/task-parent.jsonl` fixture, kept here
//! verbatim so the case is pinned after the fixture itself was deleted.

mod common;

use common::TempDir;
use radian_core::Message;
use radian_session::{Session, SessionJournal};

/// The pre-change lead journal: `SessionCreated`, a turn, one `TaskSpawned` and one
/// `TaskFinished` (both non-critical), and the two transcript messages.
const OLD_LEAD_JOURNAL: &str = concat!(
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":0,"ts":"2026-10-15T09:00:00.000000Z","event":"SessionCreated","critical":true,"data":{"workspace":"/work/example","config_snapshot":{"model":{"provider":"example","model":"m-1"}}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":1,"ts":"2026-10-15T09:00:01.000000Z","event":"TurnStarted","critical":true,"data":{"turn_id":"t-1"}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":2,"ts":"2026-10-15T09:00:02.000000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[{"text":{"text":"spawn a task for slice 1"}}]}}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":3,"ts":"2026-10-15T09:00:03.000000Z","event":"TaskSpawned","critical":false,"data":{"agent_session_id":"14222222-2222-4222-8222-222222222222","worktree":"/work/radian-home/worktrees/example-0f3a1c9b7d2e/task-1","label":"slice 1"}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":4,"ts":"2026-10-15T09:00:04.000000Z","event":"MessageAppended","critical":true,"data":{"index":1,"message":{"assistant":{"content":[{"text":{"text":"spawned task-1"}}]}}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":5,"ts":"2026-10-15T09:00:05.000000Z","event":"TurnFinished","critical":true,"data":{"turn_id":"t-1","outcome":"completed"}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14111111-1111-4111-8111-111111111111","seq":6,"ts":"2026-10-15T09:10:00.000000Z","event":"TaskFinished","critical":false,"data":{"agent_session_id":"14222222-2222-4222-8222-222222222222","outcome":"completed"}}"#,
    "\n",
);

/// The pre-change task agent's own journal: `SessionCreated.parent` carries `relation: "task"`
/// and one brief message. `SessionCreated` is stored opaquely, so this opens
/// regardless; the case pins that the removed relation value still reads.
const OLD_TASK_CHILD_JOURNAL: &str = concat!(
    r#"{"schema_version":1,"session_id":"14222222-2222-4222-8222-222222222222","seq":0,"ts":"2026-10-15T09:00:03.100000Z","event":"SessionCreated","critical":true,"data":{"workspace":"/work/example/.radian-worktrees/task-1","config_snapshot":{"model":{"provider":"example","model":"m-1"}},"parent":{"session_id":"14111111-1111-4111-8111-111111111111","seq":3,"relation":"task"}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"14222222-2222-4222-8222-222222222222","seq":1,"ts":"2026-10-15T09:00:03.200000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[{"text":{"text":"Fix the typo in a.txt and commit it."}}]}}}}"#,
    "\n",
);

/// The reader opens the old journal, skips the two task events as unknown non-critical, and
/// folds the transcript exactly as before.
#[test]
fn an_old_lead_journal_with_task_events_still_opens_and_replays() {
    let dir = TempDir::new("old-lead");
    let path = dir.file("lead.jsonl");
    std::fs::write(&path, OLD_LEAD_JOURNAL).unwrap();

    let session = Session::open(&path).unwrap();
    assert_eq!(session.session_id(), "14111111-1111-4111-8111-111111111111");
    // Every line is accepted; the two task events are skipped, in order.
    assert_eq!(
        session.skipped(),
        &[
            (3, "TaskSpawned".to_string()),
            (6, "TaskFinished".to_string())
        ],
        "the task events must be reported as skipped, not refused"
    );
    assert_eq!(session.events().len(), 5, "5 of the 7 lines are known");
    // The transcript fold is unaffected: the two messages, in order.
    assert_eq!(
        session.history().unwrap(),
        [
            Message::user("spawn a task for slice 1"),
            Message::assistant("spawned task-1"),
        ]
    );
}

/// The old task agent's own journal still opens: `SessionCreated` is stored
/// opaquely, so the removed `relation: "task"` value is preserved and the fold is unaffected.
#[test]
fn an_old_task_child_journal_still_opens() {
    let dir = TempDir::new("old-task-child");
    let path = dir.file("child.jsonl");
    std::fs::write(&path, OLD_TASK_CHILD_JOURNAL).unwrap();

    let session = Session::open(&path).unwrap();
    assert!(session.skipped().is_empty());
    assert_eq!(session.events().len(), 2);
    assert_eq!(
        session.history().unwrap(),
        [Message::user("Fix the typo in a.txt and commit it.")]
    );
}

/// The **resume** path (the host takes this on `--resume`) opens the same file, keeps its
/// `seq`, and appends after it.
#[tokio::test]
async fn the_resume_path_opens_the_old_journal_and_appends() {
    let dir = TempDir::new("old-lead-resume");
    let path = dir.file("lead.jsonl");
    std::fs::write(&path, OLD_LEAD_JOURNAL).unwrap();

    let journal = SessionJournal::resume(&path, "14111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        journal.next_seq(),
        7,
        "the resumed writer continues the seq"
    );
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnFailed {
            turn_id: "t-2".into(),
            message: "boom".into(),
        })
        .await
        .unwrap();
    journal.close().unwrap();

    // The file now has the old lines plus one; the reader still opens it and still skips the
    // two task events.
    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.skipped(),
        &[
            (3, "TaskSpawned".to_string()),
            (6, "TaskFinished".to_string())
        ]
    );
    assert_eq!(session.events().len(), 6);
}
