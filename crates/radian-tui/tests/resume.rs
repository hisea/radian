//! `--resume` applies the session workspace rule (ADR 0006 §4) before the terminal is entered:
//! a session created in another workspace is refused with the ordinary startup error (exit 2)
//! and nothing is written to its journal. The refusal happens on the normal screen, so this test
//! runs the binary as an ordinary command, like `list.rs`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
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

/// A stand-in hub-held process: a control socket at `<home>/run/<test pid>.sock` whose
/// `list_sessions` reports `id` as `holder: hub`. It keeps answering after the child exits,
/// which is how the test proves the refusal left the holder untouched. The socket is named
/// with the **test process's** pid (alive) so a concurrent app's stale-socket cleanup leaves
/// it alone.
fn start_fake_hub(home: &Path, id: &str, workspace: &Path) -> (PathBuf, u32) {
    let run = home.join("run");
    std::fs::create_dir_all(&run).unwrap();
    let pid = std::process::id();
    let socket = run.join(format!("{pid}.sock"));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).unwrap();
    let id = id.to_owned();
    let workspace = std::fs::canonicalize(workspace)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(copy) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(copy);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let answer = serde_json::json!({
                "id": 1, "ok": true,
                "sessions": [{
                    "id": id,
                    "kind": "main",
                    "holder": "hub",
                    "workspace": workspace,
                    "model": {"provider": "anthropic", "model": "m"},
                    "state": "idle",
                    "pending": 0,
                }],
            });
            let mut bytes = answer.to_string().into_bytes();
            bytes.push(b'\n');
            let _ = stream.write_all(&bytes);
        }
    });
    (socket, pid)
}

/// One `list_sessions` request on the fake hub's socket, to show it still answers.
fn fake_hub_answer(socket: &Path) -> serde_json::Value {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"{\"id\":9,\"verb\":\"list_sessions\"}\n")
        .unwrap();
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).unwrap();
    serde_json::from_str(answer.trim()).unwrap()
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

/// Edge 4 / ADR 0048 §16: `--resume` of a session a hub-held process hosts is refused before
/// the journal is locked, naming the hub and its pid, the same kind of message as the
/// other-workspace refusal. The holder keeps running and the journal is unchanged.
#[tokio::test]
async fn resume_refuses_a_session_a_hub_held_process_hosts() {
    let dir = TempDir::new("hub-held");
    // A control socket path must stay under `SUN_LEN` (104 bytes on macOS), so `RADIAN_HOME`
    // is the worktree's short `.radian` instead of the test's descriptive temp directory.
    let home = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the worktree root")
        .join(".radian");
    let sessions = dir.path().join("sessions");
    let ws = dir.path().join("ws");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let _ = std::fs::remove_file(home.join(format!("run/{}.sock", std::process::id())));
    let session = write_session(&sessions, "held", &ws, "held by the hub").await;
    let before = std::fs::read(&session).unwrap();
    let (socket, holder_pid) = start_fake_hub(&home, "held", &ws);

    let output = run_resume(&home, &sessions, &ws, "held");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("held by the hub")
            && stderr.contains("held")
            && stderr.contains(&format!("pid {holder_pid}")),
        "the refusal names the holder and its pid: {stderr}"
    );
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "a refused resume writes nothing to the journal"
    );
    // The hub-held process is untouched: it still answers its control port.
    let answer = fake_hub_answer(&socket);
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(answer["sessions"][0]["id"], "held", "{answer}");
    let _ = std::fs::remove_file(&socket);
}
