//! `--list` prints this workspace's sessions and exits **without** touching the terminal, so
//! this test runs the binary as an ordinary command.

use std::path::PathBuf;
use std::process::Command;

use agent_core::{Message, TranscriptJournal};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-list-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a one-message session for `workspace`. The workspace is canonicalized first: the
/// app canonicalizes `--workspace`, and on macOS the temp dir is reached through a symlink.
async fn write_session(dir: &std::path::Path, id: &str, workspace: &std::path::Path) {
    let workspace = std::fs::canonicalize(workspace).expect("canonicalize the workspace");
    let journal = SessionJournal::create(
        dir.join(format!("{id}.jsonl")),
        id,
        SessionCreatedData::new(workspace.to_string_lossy(), json!({})),
    )
    .expect("create the session file");
    journal
        .append(agent_core::JournalRecord::Message {
            index: 0,
            message: Message::user("hello"),
        })
        .await
        .unwrap();
}

fn run_list(dir: &std::path::Path, workspace: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_radian-tui"))
        .args([
            "--model",
            "mock",
            "--session-dir",
            dir.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--list",
        ])
        .output()
        .expect("run --list");
    assert!(
        output.status.success(),
        "--list failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn list_prints_this_workspaces_sessions_and_exits() {
    let dir = TempDir::new("list");
    let ws = dir.path().join("ws");
    let other = dir.path().join("other");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    write_session(dir.path(), "list-me", &ws).await;
    write_session(dir.path(), "elsewhere", &other).await;

    let out = run_list(dir.path(), &ws);
    assert!(out.contains("list-me"), "{out}");
    assert!(
        !out.contains("elsewhere"),
        "a session from another workspace must not be listed: {out}"
    );
    // Four tab-separated columns, like `agent-cli --list`.
    let line = out.lines().find(|l| l.contains("list-me")).unwrap();
    assert_eq!(line.split('\t').count(), 4, "{line:?}");
}

/// A `.draft` file next to the sessions is not a session: `--list` (and the index rebuild) only
/// reads `*.jsonl`.
#[tokio::test]
async fn a_draft_file_is_not_listed_as_a_session() {
    let dir = TempDir::new("draft-list");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(dir.path(), "real", &ws).await;
    std::fs::write(dir.path().join("real.draft"), "unsent").unwrap();
    std::fs::write(dir.path().join("ghost.draft"), "also unsent").unwrap();

    let out = run_list(dir.path(), &ws);
    assert!(out.contains("real"), "{out}");
    assert!(
        !out.contains("ghost"),
        "a .draft must not be a session: {out}"
    );
    assert!(!out.contains("draft"), "no .draft row: {out}");
}

/// A stdout whose reader is already gone: the child's first write is `EPIPE`, deterministically
/// (no dependence on the pipe buffer size). `SIGPIPE` stays ignored (Rust's default).
#[cfg(unix)]
fn closed_stdout() -> std::process::Stdio {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: a fresh pipe; both fds are owned here.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: closing a fd we own.
    unsafe { libc::close(fds[0]) };
    // SAFETY: `fds[1]` is an owned write fd, moved into the child's stdout.
    unsafe { std::process::Stdio::from_raw_fd(fds[1]) }
}

/// D21: `radian-tui --list | head -1` must not panic; a closed stdout is a normal stop.
#[cfg(unix)]
#[tokio::test]
async fn a_closed_stdout_is_not_a_panic() {
    let dir = TempDir::new("list-sigpipe");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(dir.path(), "sigpipe", &ws).await;

    let mut child = Command::new(env!("CARGO_BIN_EXE_radian-tui"))
        .args([
            "--model",
            "mock",
            "--session-dir",
            dir.path().to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--list",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(closed_stdout())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn radian-tui --list");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}
