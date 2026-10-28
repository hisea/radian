//! `--resume` applies the session workspace rule (ADR 0006 §4) before the terminal is entered:
//! a session created in another workspace is refused with the ordinary startup error (exit 2)
//! and nothing is written to its journal. The refusal happens on the normal screen, so this test
//! runs the binary as an ordinary command, like `list.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use radian_core::{Message, TranscriptJournal};
use radian_session::{SessionCreatedData, SessionJournal};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-resume-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A session for `workspace`. The workspace is canonicalized first: the app canonicalizes
/// `--workspace`, and on macOS the temp dir is reached through a symlink.
async fn write_session(dir: &Path, id: &str, workspace: &Path, user: &str) -> PathBuf {
    let workspace = std::fs::canonicalize(workspace).expect("canonicalize the workspace");
    let path = dir.join(format!("{id}.jsonl"));
    let journal = SessionJournal::create(
        &path,
        id,
        SessionCreatedData::new(workspace.to_string_lossy(), json!({})),
    )
    .expect("create the session file");
    journal
        .append(radian_core::JournalRecord::Message {
            index: 0,
            message: Message::user(user),
        })
        .await
        .unwrap();
    journal
        .append(radian_core::JournalRecord::Message {
            index: 1,
            message: Message::assistant("reply"),
        })
        .await
        .unwrap();
    drop(journal);
    path
}

fn run_resume(home: &Path, sessions: &Path, workspace: &Path, id: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_radian"))
        .args([
            "--model",
            "mock",
            "--base-url",
            "http://127.0.0.1:1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
            "--resume",
            id,
        ])
        .env("RADIAN_HOME", home)
        .env_remove("RADIAN_SESSION_DIR")
        .current_dir(home)
        .output()
        .expect("run radian --resume")
}

/// ADR 0006 §4: `--resume` of a session created in another workspace is refused by name, before
/// the terminal is entered, and the journal is unchanged. The pre-change app resumed it.
#[tokio::test]
async fn resume_refuses_another_workspaces_session() {
    let dir = TempDir::new("other-ws");
    let sessions = dir.path().join("sessions");
    let ws_a = dir.path().join("ws-a");
    let ws_b = dir.path().join("ws-b");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let session = write_session(&sessions, "from-a", &ws_a, "hello from a").await;
    let before = std::fs::read(&session).unwrap();

    let output = run_resume(dir.path(), &sessions, &ws_b, "from-a");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("from-a") && stderr.contains("not in this workspace"),
        "the error names the session and the rule: {stderr}"
    );
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "a refused resume writes nothing to the journal"
    );
}
