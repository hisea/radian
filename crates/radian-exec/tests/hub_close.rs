//! The owner `close` verb on a hub-held process (ADR 0043): the port answers `ok` first, the
//! host ends the process the way the idle window does, and the journal stays for the next
//! prompt.
//!
//! Every test runs a real `radian exec --hub-held` child under a private `RADIAN_HOME` and
//! drives its control socket, so the verb, the host loop and the process exit are exercised
//! end to end.

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion.
const WATCHDOG: Duration = Duration::from_secs(60);
const TOKEN: &str = "owner-token";

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

fn question_reply() -> Reply {
    tool_reply(
        "call_ask",
        "ask_user",
        &json!({"questions": [{
            "id": "name",
            "prompt": "Name?",
            "kind": "single",
            "options": [{"id": "a", "label": "A"}],
            "allow_other": true,
            "required": true
        }]})
        .to_string(),
    )
}

/// A hub-held `radian exec` under a private home. Dropping it kills the child; the scratch
/// tree is removed when the test process exits.
struct HubHeld {
    child: Option<Child>,
    sessions: PathBuf,
    id: String,
    socket: PathBuf,
    log: PathBuf,
}

impl HubHeld {
    async fn start(label: &str, base_url: &str, idle_secs: u64) -> Self {
        let root =
            common::process_dir_in(Path::new("/tmp"), &format!("radian-exec-hub-close-{label}"));
        let home = root.join("home");
        let sessions = root.join("sessions");
        let workspace = root.join("ws");
        for dir in [&home, &sessions, &workspace] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let hub = home.join("hub");
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::write(hub.join("token"), TOKEN).unwrap();
        let id = format!("hub-close-{label}");
        let log = root.join("child.log");
        let stderr = std::fs::File::create(&log).expect("the child log is creatable");
        let idle = idle_secs.to_string();
        let workspace = workspace.to_string_lossy().into_owned();
        let child = Command::new(common::radian_bin())
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("RADIAN_HOME", &home)
            .env("RADIAN_SESSION_DIR", &sessions)
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .current_dir(&workspace)
            .args([
                "exec",
                "--hub-held",
                "--hub-idle-secs",
                &idle,
                "--workspace",
                &workspace,
                "--session-id",
                &id,
                "--model",
                "mock",
                "--base-url",
                base_url,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .expect("spawn radian exec --hub-held");
        let socket = home
            .join("run")
            .join(format!("{}.sock", child.id().expect("the child has a pid")));
        wait_until("the hub-held session's socket answers", || {
            sessions.join(format!("{id}.jsonl")).is_file()
                && std::os::unix::net::UnixStream::connect(&socket).is_ok()
        })
        .await;
        Self {
            child: Some(child),
            sessions,
            id,
            socket,
            log,
        }
    }

    fn journal_path(&self) -> PathBuf {
        self.sessions.join(format!("{}.jsonl", self.id))
    }

    fn journal(&self) -> Vec<Value> {
        std::fs::read_to_string(self.journal_path())
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn wait_journal(&self, want: impl Fn(&Value) -> bool) {
        wait_until("the journal line", || self.journal().iter().any(&want)).await;
    }

    /// Waits for the child to exit and returns its code and stderr log.
    async fn finish(&mut self) -> (i32, String) {
        let mut child = self.child.take().expect("the child was not finished yet");
        let status = timeout(WATCHDOG, child.wait())
            .await
            .expect("the hub-held process did not exit")
            .expect("wait for the hub-held child");
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        (status.code().unwrap_or(-1), log)
    }
}

/// Polls `predicate` until it holds, bounded.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    timeout(WATCHDOG, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

struct Client {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl Client {
    async fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path)
            .await
            .expect("connect the control socket");
        let (read, write) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer: write,
        }
    }

    async fn send(&mut self, value: Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.writer
            .write_all(&line)
            .await
            .expect("write the request");
        self.writer.flush().await.expect("flush the request");
    }

    async fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = timeout(WATCHDOG, self.reader.read_line(&mut line))
            .await
            .expect("the answer never arrived")
            .expect("read the answer");
        assert!(n > 0, "the connection closed before an answer arrived");
        serde_json::from_str(line.trim_end()).expect("a JSON answer")
    }

    async fn request(&mut self, value: Value) -> Value {
        self.send(value).await;
        self.read().await
    }
}

fn close_request(id: u64) -> Value {
    json!({"id": id, "verb": "close", "token": TOKEN})
}

/// One close attempt that tolerates a socket that is already gone: `None` when the connect or
/// the answer fails, which is what an exited process looks like.
async fn try_close(socket: &Path) -> Option<Value> {
    let stream = UnixStream::connect(socket).await.ok()?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(&close_request(1)).unwrap();
    line.push(b'\n');
    write.write_all(&line).await.ok()?;
    write.flush().await.ok()?;
    let mut reader = BufReader::new(read);
    let mut answer = String::new();
    let n = timeout(Duration::from_millis(500), reader.read_line(&mut answer))
        .await
        .ok()?
        .ok()?;
    if n == 0 {
        return None;
    }
    serde_json::from_str(answer.trim_end()).ok()
}

#[tokio::test]
async fn close_ends_a_hub_held_process_cleanly_and_the_journal_stays() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let mut session = HubHeld::start("clean", &server.base_url, 60).await;
    let mut client = Client::connect(&session.socket).await;

    let followed = client
        .request(json!({"id": 1, "verb": "followup", "text": "hello", "token": TOKEN}))
        .await;
    assert_eq!(followed["ok"], true, "{followed}");
    session
        .wait_journal(|line| line["event"] == "TurnFinished")
        .await;

    let answered = client.request(close_request(2)).await;
    assert_eq!(answered["ok"], true, "{answered}");
    let (code, stderr) = session.finish().await;
    assert_eq!(code, 0, "the owner's close exits cleanly: {stderr}");
    assert!(!session.socket.exists(), "the socket is removed on exit");
    let journal = session.journal();
    assert!(
        journal.iter().any(|line| line["event"] == "SessionCreated"),
        "the journal stays: {journal:?}"
    );
    assert!(
        journal
            .iter()
            .any(|line| line["event"] == "TurnFinished" && line["data"]["outcome"] == "completed"),
        "the finished turn is still there: {journal:?}"
    );
}

#[tokio::test]
async fn close_abandons_a_pending_question_and_ends_the_process() {
    let server = MockServer::start(vec![question_reply(), text_reply("done")]).await;
    let mut session = HubHeld::start("question", &server.base_url, 60).await;
    let mut client = Client::connect(&session.socket).await;

    let followed = client
        .request(json!({"id": 1, "verb": "followup", "text": "ask", "token": TOKEN}))
        .await;
    assert_eq!(followed["ok"], true, "{followed}");
    // The `ask_user` tool call is open and waiting on the pending queue: this is the
    // "open tool call" the close must cancel and settle.
    session
        .wait_journal(|line| {
            line["event"] == "ToolExecutionStarted" && line["data"]["call_id"] == "call_ask"
        })
        .await;

    let answered = client.request(close_request(2)).await;
    assert_eq!(answered["ok"], true, "{answered}");
    let (code, stderr) = session.finish().await;
    assert_eq!(code, 0, "the owner's close exits cleanly: {stderr}");

    let journal = session.journal();
    assert!(
        journal
            .iter()
            .any(|line| line["event"] == "ToolExecutionStarted"
                && line["data"]["call_id"] == "call_ask"),
        "{journal:?}"
    );
    assert!(
        journal
            .iter()
            .any(|line| line["event"] == "ToolExecutionSettled"
                && line["data"]["call_id"] == "call_ask"
                && line["data"]["result"]["error_code"] == "cancelled"),
        "the open call is settled as cancelled (ADR 0002 E5): {journal:?}"
    );
    assert!(
        journal
            .iter()
            .any(|line| line["event"] == "TurnFinished" && line["data"]["outcome"] == "cancelled"),
        "the waiting turn is cancelled: {journal:?}"
    );
}

#[tokio::test]
async fn two_close_requests_at_once_both_get_ok_and_the_process_exits_once() {
    let server = MockServer::start(Vec::new()).await;
    let mut session = HubHeld::start("twice", &server.base_url, 60).await;
    let mut first = Client::connect(&session.socket).await;
    let mut second = Client::connect(&session.socket).await;
    // Both connections are served before the close: the accept loop stops on the drain, so a
    // connection still in the backlog could not be answered.
    for client in [&mut first, &mut second] {
        let listed = client
            .request(json!({"id": 0, "verb": "list_sessions"}))
            .await;
        assert_eq!(listed["ok"], true, "{listed}");
    }

    first.send(close_request(1)).await;
    second.send(close_request(2)).await;
    let first_answer = first.read().await;
    let second_answer = second.read().await;
    assert_eq!(first_answer["ok"], true, "{first_answer}");
    assert_eq!(second_answer["ok"], true, "{second_answer}");

    let (code, stderr) = session.finish().await;
    assert_eq!(code, 0, "one clean exit: {stderr}");
}

#[tokio::test]
async fn close_and_the_idle_window_race_to_one_clean_exit() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    // A short idle window: the process may exit through it, through the close, or both paths
    // notice at once. Any of those is one clean exit, never a second driver join.
    let mut session = HubHeld::start("race", &server.base_url, 5).await;
    let mut client = Client::connect(&session.socket).await;
    let followed = client
        .request(json!({"id": 1, "verb": "followup", "text": "hello", "token": TOKEN}))
        .await;
    assert_eq!(followed["ok"], true, "{followed}");
    session
        .wait_journal(|line| line["event"] == "TurnFinished")
        .await;

    // Most of the idle window has passed before the first attempt: the closes straddle the
    // moment the loop would exit on idleness, so the two paths really race.
    tokio::time::sleep(Duration::from_millis(4_800)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    while tokio::time::Instant::now() < deadline {
        if try_close(&session.socket).await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let (code, stderr) = session.finish().await;
    assert_eq!(code, 0, "the race still ends once, cleanly: {stderr}");
}
