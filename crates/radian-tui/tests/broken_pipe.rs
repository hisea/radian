//! No command panics when its stdout pipe closes early: a command that writes to
//! stdout and finds the reader gone stops writing and exits 0, with no panic and no
//! error line (like `head`, `git` and `ripgrep`). Any other write error exits 1 with
//! one stderr line. Each broken-pipe test below spawns the real binary with a stdout
//! whose reader is already gone, so the first write fails deterministically.

mod common;

use std::process::{Command, Stdio};

/// A unique scratch directory for one test.
fn scratch(name: &str) -> common::TempDir {
    common::TempDir::named(&format!("radian-broken-pipe-{name}"))
}

/// Stdout whose reader is already gone: the first write fails instead of blocking, with
/// no race against the child's startup (the read end is closed before the spawn).
fn closed_stdout() -> Stdio {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    let (reader, writer) = UnixStream::pair().expect("socketpair");
    drop(reader);
    let fd = writer.into_raw_fd();
    // SAFETY: `fd` is the open write end of the pair; ownership moves to the child.
    unsafe { Stdio::from_raw_fd(fd) }
}

/// Spawns the `radian` binary with `args`, a closed stdout, a null stdin and an empty
/// `RADIAN_HOME`, and waits for it. Returns `(exit code, stderr text)`.
fn run_closed(args: &[&str], home: &std::path::Path, cwd: &std::path::Path) -> (i32, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_radian"))
        .args(args)
        .env("RADIAN_HOME", home)
        .env_remove("RADIAN_SESSION_DIR")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(closed_stdout())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn radian");
    let output = child.wait_with_output().expect("wait for radian");
    let code = output.status.code().unwrap_or(-1);
    (code, String::from_utf8_lossy(&output.stderr).into_owned())
}

/// A closed output pipe is a normal stop: exit 0, no panic, no error line.
fn assert_quiet_stop(test: &str, code: i32, stderr: &str) {
    assert_eq!(code, 0, "{test}: exit code (stderr: {stderr:?})");
    assert!(
        !stderr.contains("panicked"),
        "{test}: stderr names a panic: {stderr:?}"
    );
}

/// `radian docs` (the topic index) with a closed stdout: exit 0, no panic.
#[test]
fn closed_pipe_docs_index_exits_zero_without_panic() {
    let base = scratch("docs-index");
    let (code, stderr) = run_closed(&["docs"], &base, &base);
    assert_quiet_stop("docs", code, &stderr);
}

/// `radian docs <topic>` with a closed stdout: exit 0, no panic.
#[test]
fn closed_pipe_docs_topic_exits_zero_without_panic() {
    let base = scratch("docs-topic");
    let (code, stderr) = run_closed(&["docs", "footer"], &base, &base);
    assert_quiet_stop("docs footer", code, &stderr);
}

/// `radian footer export deck` with a closed stdout: exit 0, no panic.
#[test]
fn closed_pipe_footer_export_exits_zero_without_panic() {
    let base = scratch("footer-export");
    let (code, stderr) = run_closed(&["footer", "export", "deck"], &base, &base);
    assert_quiet_stop("footer export deck", code, &stderr);
}

/// `radian memory list` against an empty `RADIAN_HOME` with a closed stdout:
/// exit 0, no panic.
#[test]
fn closed_pipe_memory_list_exits_zero_without_panic() {
    let base = scratch("memory-list");
    let (code, stderr) = run_closed(&["memory", "list"], &base, &base);
    assert_quiet_stop("memory list", code, &stderr);
}

/// `radian exec --list` over a session directory holding one session, with a closed
/// stdout: exit 0, no panic.
#[test]
fn closed_pipe_exec_list_exits_zero_without_panic() {
    let base = scratch("exec-list");
    let home = base.join("home");
    let sessions = base.join("sessions");
    let work = base.join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let workspace = work.canonicalize().unwrap().to_string_lossy().into_owned();
    let line = |seq: u64, event: &str, data: serde_json::Value| {
        format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1,
                "session_id": "seed01",
                "seq": seq,
                "ts": "2026-10-20T00:00:00Z",
                "event": event,
                "critical": true,
                "data": data,
            })
        )
    };
    let mut contents = line(
        0,
        "SessionCreated",
        serde_json::json!({"workspace": workspace, "config_snapshot": {}}),
    );
    contents.push_str(&line(
        1,
        "MessageAppended",
        serde_json::json!({
            "index": 0,
            "message": {"user": {"content": [{"text": {"text": "hello"}}]}},
        }),
    ));
    std::fs::write(sessions.join("seed01.jsonl"), contents).unwrap();
    let session_dir = sessions.to_string_lossy().into_owned();
    let (code, stderr) = run_closed(
        &["exec", "--list", "--session-dir", &session_dir],
        &home,
        &work,
    );
    assert_quiet_stop("exec --list", code, &stderr);
}

/// An unknown docs topic exits 2 and names the topic in backticks before the list.
#[test]
fn unknown_docs_topic_message_names_the_topic_in_backticks() {
    let base = scratch("unknown-topic");
    let output = Command::new(env!("CARGO_BIN_EXE_radian"))
        .args(["docs", "no-such-topic-xyz"])
        .env("RADIAN_HOME", &base)
        .env_remove("RADIAN_SESSION_DIR")
        .current_dir(&base)
        .stdin(Stdio::null())
        .output()
        .expect("run radian docs");
    assert_eq!(output.status.code(), Some(2), "exit code");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        stderr.starts_with("radian: unknown docs topic `no-such-topic-xyz`; available topics: "),
        "the message names the topic in backticks: {stderr:?}"
    );
    assert!(output.stdout.is_empty(), "nothing on stdout");
}
