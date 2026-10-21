//! ADR 0006-3/6: torn tails and crash recovery, plus the single-writer guarantee.
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
//! sibling is forking. A host that does reopen a journal while a child it forked is between
//! `fork` and `exec` is covered instead by the writer lock's bounded retry
//! ([`SessionJournal::open_locked`]): the false `Locked` is transient, so `open_locked`
//! retries it for ~200 ms before reporting a genuine second writer.
//!
//! Only tests in **this** binary can interfere: each integration-test file is its own
//! process, and a child inherits only its own parent's descriptors. Only this file spawns
//! children, so the lock lives here rather than in `common`.

mod common;

use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use radian_core::{JournalRecord, Message, TranscriptJournal};
use common::{TempDir, created};
use radian_session::{Session, SessionJournal, WriteError, lock_retry_attempts};

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

// The body awaits a journal append while holding the serial lock on purpose; the lock
// serializes these tests and nothing else in this test's runtime needs it.
#[allow(clippy::await_holding_lock)]
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

// The body awaits a journal append while holding the serial lock on purpose; the lock
// serializes these tests and nothing else in this test's runtime needs it.
#[allow(clippy::await_holding_lock)]
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

// ---------------------------------------------------------------------------------------
// The writer lock's bounded retry (the transient false `Locked` from fork inheritance)
// ---------------------------------------------------------------------------------------

/// Waits until the writer lock has been retried more than `base` times, so a test can prove
/// `open_locked` is in its retry loop before it frees a deliberately held lock. Bounded by a
/// watchdog: if no retry ever happens the test fails instead of hanging.
#[cfg(unix)]
fn wait_for_lock_retry(base: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock_retry_attempts() <= base {
        assert!(
            Instant::now() < deadline,
            "the writer lock was never retried: the inherited lock was not held, or the retry is gone"
        );
        std::thread::yield_now();
    }
}

/// The lock a forked child inherited is **transient**: the child shares the parent's open
/// file description and releases the `flock` only when that descriptor closes (at `exec`, or
/// when the child exits). This hands a child the *exact* writer-lock descriptor — a `dup`
/// with `FD_CLOEXEC` cleared, so it survives the child's `exec` — drops the parent's own
/// handles, starts `resume`, and only then lets the child exit. `resume` must ride out the
/// window and succeed; without the retry it sees a false `Locked` and fails.
#[cfg(unix)]
#[test]
fn a_lock_inherited_by_a_forked_child_is_retried_and_succeeds() {
    use std::os::fd::{FromRawFd, OwnedFd};

    let _serial = serial();
    let dir = TempDir::new("lock-inherit");
    let path = dir.file("s.jsonl");
    let sid = "sid-inherit";
    {
        let journal = SessionJournal::create(&path, sid, created()).unwrap();
        drop(journal);
    }

    // Take the writer lock, then `dup` it with `FD_CLOEXEC` cleared so the forked child
    // inherits this exact open file description and keeps the lock across its `exec`.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(&path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let dup = unsafe { libc::dup(held.as_raw_fd()) };
    assert!(dup >= 0, "dup of the writer-lock descriptor failed");
    let flags = unsafe { libc::fcntl(dup, libc::F_GETFD) };
    assert!(flags >= 0, "fcntl(F_GETFD) failed");
    unsafe { libc::fcntl(dup, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    let inherited = unsafe { OwnedFd::from_raw_fd(dup) };

    // The child parks on its stdin holding the inherited descriptor; it prints `HELD` once it
    // has `exec`ed, so the parent knows the lock now outlives the parent's own handles.
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env("RADIAN_HOLD_INHERITED_LOCK", "1")
        .args(["--exact", "child_holds_inherited_lock", "--nocapture"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    // The harness prints a leading blank line and its banner before the helper's output.
    let mut line = String::new();
    loop {
        line.clear();
        let read = stdout.read_line(&mut line).unwrap();
        assert!(read > 0, "the helper exited before reporting HELD");
        if line.trim() == "HELD" {
            break;
        }
    }

    // From here the parent holds nothing: only the child's inherited descriptor keeps the
    // lock, exactly as if another thread's `git` subprocess were between fork and exec.
    drop(inherited);
    drop(held);

    let base = lock_retry_attempts();
    let resume_path = path.clone();
    let resumed = std::thread::spawn(move || SessionJournal::resume(&resume_path, sid));
    // Prove `resume` is retrying before releasing the child: no race against its first try.
    wait_for_lock_retry(base);
    // Release the child inside the window. It exits, the inherited descriptor closes, and the
    // lock is free for `resume`'s next attempt.
    child.stdin.take().unwrap().write_all(b"x").unwrap();
    match resumed.join().unwrap() {
        Ok(journal) => assert_eq!(journal.next_seq(), 1),
        Err(other) => panic!("resume must succeed once the inherited lock is released: {other:?}"),
    }
    let _ = child.wait();
}

/// Child for `a_lock_inherited_by_a_forked_child_is_retried_and_succeeds`. No-op unless
/// spawned. It inherits the parent's writer-lock descriptor (with `FD_CLOEXEC` cleared) and
/// holds it until the parent writes a byte to its stdin.
#[cfg(unix)]
#[test]
fn child_holds_inherited_lock() {
    use std::io::Read;

    if std::env::var_os("RADIAN_HOLD_INHERITED_LOCK").is_none() {
        return;
    }
    println!("HELD");
    std::io::stdout().flush().unwrap();
    let mut byte = [0u8; 1];
    let _ = std::io::stdin().read_exact(&mut byte);
}

/// A genuine second writer holds the lock for as long as it lives — far past the retry
/// budget — so it is still refused, and the refusal arrives within a bounded time instead of
/// the retry becoming unbounded.
#[cfg(unix)]
#[test]
fn a_held_lock_is_still_refused_after_the_retry_budget() {
    let _serial = serial();
    let dir = TempDir::new("lock-budget");
    let path = dir.file("s.jsonl");
    let sid = "sid-budget";
    {
        let journal = SessionJournal::create(&path, sid, created()).unwrap();
        drop(journal);
    }
    let held = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(&path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );

    let (tx, rx) = mpsc::channel();
    let resume_path = path.clone();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(SessionJournal::resume(&resume_path, sid));
    });
    let started = Instant::now();
    let result = rx.recv_timeout(Duration::from_secs(2));
    let elapsed = started.elapsed();
    // Free the lock before asserting so a still-retrying worker can finish either way.
    drop(held);
    let result = result.expect("a second writer must be refused within the budget, not hang");
    match result {
        Err(WriteError::Locked { .. }) => {}
        Err(other) => panic!("the second writer must be refused, got {other:?}"),
        Ok(_) => panic!("the second writer acquired the lock"),
    }
    assert!(
        elapsed < Duration::from_millis(1500),
        "the refusal must come from the bounded retry budget, took {elapsed:?}"
    );
    let _ = worker.join();
}
