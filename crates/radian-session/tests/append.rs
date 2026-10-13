//! ADR 0006 §15-3/6: torn tails and crash recovery, plus the single-writer guarantee.
//!
//! The crash test spawns this same test binary (the `#[test]` functions whose names start
//! with `child_` are no-ops unless the parent sets an env var), has the child write a
//! genuine partial line, and then `kill -9`s it (Rust's `Child::kill` sends SIGKILL on
//! Unix).
//!
//! # Why these tests take a process-wide lock
//!
//! `flock` is attached to the **open file description**. When one test spawns a child
//! process, the child inherits the parent's file descriptors until it `exec`s; an inherited
//! descriptor shares the parent's open file description, so the parent's `flock` stays held
//! for the whole `[fork, exec)` window even after the parent closes its own descriptor. If
//! another test in the same process closes and reopens the same still-to-be-created file in
//! that window, its `try_lock_exclusive` sees a false `Locked` and fails. The window is tiny
//! but real: 1 in 100 full-binary runs here, and 4 in 12 parallel runs of just the two
//! affected tests (see the flake report in the commit that added [`SERIAL`]).
//!
//! Serializing the tests that spawn a child with the ones that hold or reopen a journal
//! closes the window: a victim can no longer be between `close` and `try_lock` while a
//! sibling is forking. Production is unaffected: a host locks the session file once at
//! startup, before any child process exists.
//!
//! Only tests in **this** binary can interfere: each integration-test file is its own
//! process, and a child inherits only its own parent's descriptors. Only this file spawns
//! children, so the lock lives here rather than in `common`.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard};

use agent_core::{JournalRecord, Message, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{Session, SessionJournal, WriteError};

fn session_id() -> &'static str {
    "sid-crash"
}

/// Serializes every test here that spawns a child or holds/reopens a journal. See the module
/// doc; the two classes must never overlap. A poisoned lock is reused (a panicking test must
/// not deadlock the rest).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tokio::test]
async fn a_torn_tail_is_truncated_before_appending() {
    let _serial = serial();
    let dir = TempDir::new("torn");
    let path = dir.file("s.jsonl");
    let sid = session_id();
    {
        let journal = SessionJournal::create(&path, sid, created()).unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("one"),
            })
            .await
            .unwrap();
    }

    // A half-written next line, exactly as a kill would leave it. It carries a marker and
    // an impossible `seq` so it cannot be a prefix of the legitimate line that follows.
    let partial =
        br#"{"schema_version":1,"session_id":"sid-crash","seq":99,"ts":"TORN_FRAGMENT_MARKER"#;
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(partial).unwrap();
        file.flush().unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert!(!before.ends_with(b"\n"));

    // Safe append: resume truncates the torn tail, then writes the next record.
    let journal = SessionJournal::resume(&path, sid).unwrap();
    assert_eq!(journal.next_seq(), 2);
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::assistant("two"),
        })
        .await
        .unwrap();

    let raw = std::fs::read(&path).unwrap();
    assert!(raw.ends_with(b"\n"));
    let marker = b"TORN_FRAGMENT_MARKER";
    assert!(
        !raw.windows(marker.len()).any(|w| w == marker),
        "the torn fragment must have been truncated away"
    );
    let session = Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [0, 1, 2]);
    assert_eq!(session.history().unwrap().len(), 2);
}

#[test]
fn a_killed_writer_leaves_only_a_torn_tail() {
    let _serial = serial();
    let dir = TempDir::new("kill");
    let path = dir.file("s.jsonl");

    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .env("RADIAN_SESSION_CRASH_PATH", &path)
        .args(["--exact", "child_crash_writer", "--nocapture"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let mut ready = false;
    for line in lines.by_ref() {
        if line.unwrap().trim() == "READY" {
            ready = true;
            break;
        }
    }
    assert!(ready, "the child never reported a partial write");
    // SIGKILL while the child is (from its point of view) still writing.
    child.kill().unwrap();
    child.wait().unwrap();

    let raw = std::fs::read(&path).unwrap();
    assert!(
        !raw.ends_with(b"\n"),
        "the killed child must have left a torn tail"
    );

    // Reopen: the complete lines survive, the torn tail is truncated away.
    let journal = SessionJournal::resume(&path, session_id()).unwrap();
    let complete = Session::open(&path);
    // `Session::open` drops the torn tail too, so it sees only the complete lines.
    let complete = complete.unwrap();
    assert!(!complete.events().is_empty());
    let last_seq = complete.events().last().unwrap().seq;
    assert_eq!(journal.next_seq(), last_seq + 1);
}

/// Child process for `a_killed_writer_leaves_only_a_torn_tail`. No-op unless spawned.
#[tokio::test]
async fn child_crash_writer() {
    let Ok(path) = std::env::var("RADIAN_SESSION_CRASH_PATH") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    {
        let journal = SessionJournal::create(&path, session_id(), created()).unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("durable one"),
            })
            .await
            .unwrap();
        journal
            .append(JournalRecord::Message {
                index: 1,
                message: Message::assistant("durable two"),
            })
            .await
            .unwrap();
    }
    // Write the first bytes of the next line and then die without the LF.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"schema_version":1,"session_id":"sid-crash","seq":3,"ts":"2026-10-12T"#)
        .unwrap();
    file.flush().unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();
    // Park until killed; no `sleep` is used as synchronization.
    loop {
        std::thread::park();
    }
}

#[tokio::test]
async fn a_second_writer_in_this_process_is_refused() {
    let _serial = serial();
    let dir = TempDir::new("lock-inproc");
    let path = dir.file("s.jsonl");
    let _first = SessionJournal::create(&path, session_id(), created()).unwrap();

    match SessionJournal::create(&path, session_id(), created()) {
        Err(WriteError::Locked { .. }) => {}
        Err(other) => panic!("the second writer must be refused, got {other:?}"),
        Ok(_) => panic!("the second writer acquired the lock"),
    }
    match SessionJournal::resume(&path, session_id()) {
        Err(WriteError::Locked { .. }) => {}
        Err(other) => panic!("resume by a second writer must be refused, got {other:?}"),
        Ok(_) => panic!("resume by a second writer acquired the lock"),
    }
}

#[test]
fn a_second_writer_in_another_process_is_refused() {
    let _serial = serial();
    let dir = TempDir::new("lock-crossproc");
    let path = dir.file("s.jsonl");
    let _first = SessionJournal::create(&path, session_id(), created()).unwrap();

    let exe = std::env::current_exe().unwrap();
    let output = Command::new(exe)
        .env("RADIAN_SESSION_LOCK_PROBE_PATH", &path)
        .args(["--exact", "child_lock_probe", "--nocapture"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("LOCKED"),
        "a second process must be refused by the file lock; child said:\n{text}"
    );
    assert!(
        !text.contains("ACQUIRED"),
        "the second process acquired the lock:\n{text}"
    );
}

/// Child process for `a_second_writer_in_another_process_is_refused`. No-op unless spawned.
#[test]
fn child_lock_probe() {
    let Ok(path) = std::env::var("RADIAN_SESSION_LOCK_PROBE_PATH") else {
        return;
    };
    match SessionJournal::create(path, session_id(), created()) {
        Err(WriteError::Locked { .. }) => println!("LOCKED"),
        Ok(_) => println!("ACQUIRED"),
        Err(other) => println!("OTHER {other:?}"),
    }
}
