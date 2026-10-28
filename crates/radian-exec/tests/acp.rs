//! `radian acp` driven as a child over stdio (ADR 0049): initialize, `session/new`, a prompt
//! with live updates and `_meta`, a permission rejection, a cancellation with the interrupted
//! journal prefix, a model error, the one-session rule, and the absent exec-only flags.
//!
//! The model is the same scripted `MockServer` the `radian exec` tests use. The client is a
//! small JSON-RPC reader/writer over the child's stdin and stdout, so the wire shapes are
//! exercised exactly as a client sees them.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use radian_exec::{ACP_MAX_INPUT_LINE_BYTES, ACP_OUTBOX_CAPACITY};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

const IO: Duration = Duration::from_secs(30);
const EXIT: Duration = Duration::from_secs(30);

fn fresh_dir(prefix: &str) -> PathBuf {
    common::process_dir(&format!("radian-acp-{prefix}"))
}

struct Env {
    home: PathBuf,
    sessions: PathBuf,
    workspace: PathBuf,
}

impl Env {
    fn fresh(prefix: &str) -> Self {
        let home = fresh_dir(prefix);
        let sessions = home.join("sessions");
        let workspace = home.join("ws");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Self {
            home,
            sessions,
            workspace,
        }
    }
}

#[derive(Clone, Copy)]
enum Decision {
    /// Answer a permission request with `reject_once`.
    Reject,
    /// Answer a permission request with this option id (an `ask_user` option).
    Select(&'static str),
    /// No permission request is expected.
    None,
    /// Send `session/cancel` after the first streamed answer chunk.
    CancelOnText,
}

struct PromptRun {
    response: Value,
    updates: Vec<Value>,
    permissions: Vec<Value>,
}

impl PromptRun {
    fn result(&self) -> &Value {
        assert!(
            self.response.get("error").is_none(),
            "session/prompt failed: {}",
            self.response
        );
        &self.response["result"]
    }
}

struct Acp {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: i64,
    /// Messages read while waiting for a response (notifications and requests).
    queued: Vec<Value>,
    stderr_path: PathBuf,
}

impl Acp {
    fn spawn(args: &[&str], env: &Env) -> Self {
        Self::spawn_with(args, env, &[])
    }

    /// [`Acp::spawn`] with extra environment for the child (the driver's commit-delay test
    /// hook needs one).
    fn spawn_with(args: &[&str], env: &Env, extra_env: &[(&str, &str)]) -> Self {
        let stderr_path = env.home.join("stderr.log");
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let mut command = Command::new(common::radian_bin());
        command
            .arg("acp")
            .args(args)
            .env_remove("OPENAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("RADIAN_CONFIG")
            .env("HOME", &env.home)
            .env("RADIAN_HOME", &env.home)
            .env("RADIAN_SESSION_DIR", &env.sessions)
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn radian acp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout")).lines();
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            queued: Vec::new(),
            stderr_path,
        }
    }

    async fn write(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).expect("serialize");
        line.push(b'\n');
        timeout(IO, self.stdin.write_all(&line))
            .await
            .expect("write timed out")
            .expect("write");
        self.stdin.flush().await.expect("flush");
    }

    /// Writes raw bytes to the child's stdin, without a newline of its own.
    async fn write_bytes(&mut self, bytes: &[u8]) {
        timeout(IO, self.stdin.write_all(bytes))
            .await
            .expect("write timed out")
            .expect("write");
    }

    /// Writes `count` `b'x'` bytes in small chunks, so the test never holds the filler whole.
    async fn write_filler(&mut self, count: usize) {
        let chunk = vec![b'x'; 64 * 1024];
        let mut left = count;
        while left > 0 {
            let take = left.min(chunk.len());
            self.write_bytes(&chunk[..take]).await;
            left -= take;
        }
        self.stdin.flush().await.expect("flush");
    }

    async fn read(&mut self) -> Value {
        if !self.queued.is_empty() {
            return self.queued.remove(0);
        }
        let line = match timeout(IO, self.stdout.next_line())
            .await
            .expect("read timed out")
            .expect("stdout read")
        {
            Some(line) => line,
            None => {
                let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
                panic!("the agent closed stdout; stderr:\n{stderr}");
            }
        };
        serde_json::from_str(&line).expect("a JSON line")
    }

    async fn send_request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await;
        id
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params).await;
        loop {
            let message = self.read().await;
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return message;
            }
            self.queued.push(message);
        }
    }

    async fn initialize(&mut self) -> Value {
        let response = self
            .request("initialize", json!({ "protocolVersion": 1 }))
            .await;
        assert!(response.get("error").is_none(), "{response}");
        response
    }

    async fn new_session(&mut self, cwd: &Path) -> Value {
        self.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
            .await
    }

    /// Sends `session/load` and collects the `session/update`s the agent replays before it
    /// answers (ADR 0049).
    async fn load_session(&mut self, session_id: &str, cwd: &Path) -> (Value, Vec<Value>) {
        let id = self
            .send_request(
                "session/load",
                json!({ "sessionId": session_id, "cwd": cwd, "mcpServers": [] }),
            )
            .await;
        let mut updates = Vec::new();
        loop {
            let message = self.read().await;
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return (message, updates);
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                assert_eq!(message["params"]["sessionId"], session_id);
                updates.push(message["params"]["update"].clone());
                continue;
            }
            self.queued.push(message);
        }
    }

    async fn prompt(&mut self, session_id: &str, text: &str, decision: Decision) -> PromptRun {
        let id = self.send_prompt(session_id, text).await;
        self.collect_prompt(id, session_id, decision).await
    }

    /// Sends one `session/prompt` and returns its request id without waiting for the answer.
    async fn send_prompt(&mut self, session_id: &str, text: &str) -> i64 {
        self.send_request(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{ "type": "text", "text": text }],
            }),
        )
        .await
    }

    /// Reads until `id`'s response, collecting the prompt's updates and answering its
    /// permission requests.
    async fn collect_prompt(&mut self, id: i64, session_id: &str, decision: Decision) -> PromptRun {
        let mut updates = Vec::new();
        let mut permissions = Vec::new();
        let mut cancelled = false;
        loop {
            let message = self.read().await;
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return PromptRun {
                    response: message,
                    updates,
                    permissions,
                };
            }
            if message.get("method").and_then(Value::as_str) == Some("session/update") {
                let update = message["params"]["update"].clone();
                if matches!(decision, Decision::CancelOnText)
                    && !cancelled
                    && update["sessionUpdate"] == "agent_message_chunk"
                    && update["content"]["text"]
                        .as_str()
                        .is_some_and(|text| !text.is_empty())
                {
                    cancelled = true;
                    self.write(json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": { "sessionId": session_id },
                    }))
                    .await;
                }
                updates.push(update);
                continue;
            }
            if message.get("method").and_then(Value::as_str) == Some("session/request_permission") {
                permissions.push(message["params"].clone());
                let reply_id = message["id"].clone();
                let outcome = match decision {
                    Decision::Reject | Decision::CancelOnText => {
                        json!({"outcome": {"outcome": "selected", "optionId": "reject_once"}})
                    }
                    Decision::Select(option) => {
                        json!({"outcome": {"outcome": "selected", "optionId": option}})
                    }
                    Decision::None => panic!("unexpected permission request: {message}"),
                };
                self.write(json!({ "jsonrpc": "2.0", "id": reply_id, "result": outcome }))
                    .await;
                continue;
            }
        }
    }

    /// Sends `session/cancel` for `session_id`, the interruption ESC produces in the TUI.
    async fn cancel(&mut self, session_id: &str) {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }))
        .await;
    }

    /// Closes stdin and waits for the process to exit **without reading stdout or dropping its
    /// read end**. A writer blocked on a full pipe therefore stays blocked, so the exit shows
    /// that the shutdown path itself does not wait for the client (ADR 0049).
    async fn exit_after_stdin_ends(mut self) -> (i32, String) {
        drop(self.stdin);
        wait_with_stdout_held(&mut self.child, &self.stderr_path).await
    }

    /// Waits for the process to exit without closing stdin and **without reading stdout or
    /// dropping its read end**, so the exit can only come from a signal (ADR 0049).
    async fn exit_while_input_stays_open(mut self) -> (i32, String) {
        wait_with_stdout_held(&mut self.child, &self.stderr_path).await
    }

    /// Sends `signal` to the child.
    #[cfg(unix)]
    fn signal(&self, signal: libc::c_int) {
        // SAFETY: signal delivery to our own child process.
        unsafe {
            libc::kill(
                self.child.id().expect("the child is running") as libc::pid_t,
                signal,
            );
        }
    }

    async fn finish(mut self) -> (i32, String) {
        drop(self.stdin);
        drop(self.stdout);
        let status = timeout(EXIT, self.child.wait())
            .await
            .expect("the agent did not exit")
            .expect("wait");
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        (status.code().unwrap_or(-1), stderr)
    }

    /// Closes stdin and reads stdout to EOF before waiting, so a test can assert that nothing
    /// was written after the input ended.
    async fn finish_reading(mut self) -> (i32, String, Vec<Value>) {
        drop(self.stdin);
        let mut messages = std::mem::take(&mut self.queued);
        loop {
            match timeout(EXIT, self.stdout.next_line()).await {
                Ok(Ok(Some(line))) => {
                    messages.push(serde_json::from_str(&line).expect("a JSON line"))
                }
                Ok(Ok(None)) | Ok(Err(_)) => break,
                Err(_) => panic!("the agent did not close stdout after stdin ended"),
            }
        }
        drop(self.stdout);
        let status = timeout(EXIT, self.child.wait())
            .await
            .expect("the agent did not exit")
            .expect("wait");
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        (status.code().unwrap_or(-1), stderr, messages)
    }
}

/// Waits for the child to exit while its stdout (and, for a signal, its stdin) stays open: a
/// writer blocked on a full pipe stays blocked, so the exit shows that the shutdown path itself
/// does not wait for the client (ADR 0049).
async fn wait_with_stdout_held(child: &mut Child, stderr_path: &Path) -> (i32, String) {
    let status = timeout(EXIT, child.wait())
        .await
        .expect("the agent did not exit while stdout was held open")
        .expect("wait");
    let stderr = std::fs::read_to_string(stderr_path).unwrap_or_default();
    (status.code().unwrap_or(-1), stderr)
}

fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: Value) -> Reply {
    let function = json!({
        "name": name,
        "arguments": serde_json::to_string(&arguments).expect("arguments"),
    });
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{
                "index": 0,
                "id": id,
                "type": "function",
                "function": function,
            }]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

/// One real background job: the `shell` tool with `background: true` (ADR 0037).
fn bg_reply(id: &str, command: &str) -> Reply {
    tool_reply(
        id,
        "shell",
        json!({ "command": command, "background": true }),
    )
}

/// Waits until the session file holds an `event` the predicate accepts. Every trigger that
/// would otherwise be a sleep-until-it-probably-happened is one of these.
async fn wait_for_event(path: &Path, what: &str, event: &str, accept: impl Fn(&Value) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let events: Vec<Value> = std::fs::read_to_string(path)
            .map(|text| {
                text.lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .collect()
            })
            .unwrap_or_default();
        if events.iter().any(|e| e["event"] == event && accept(e)) {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("the journal never held {what}; it holds:\n{events:#?}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the mock server has recorded `count` requests.
async fn wait_for_requests(server: &MockServer, count: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while server.requests().len() < count {
        if std::time::Instant::now() > deadline {
            panic!(
                "the mock never saw {what} ({} of {count} requests)",
                server.requests().len()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The message id of the `InboxEnqueued` record whose message carries `needle`.
fn enqueued_id(events: &[Value], needle: &str) -> String {
    events
        .iter()
        .find(|event| {
            event["event"] == "InboxEnqueued"
                && event["data"]["message"].to_string().contains(needle)
        })
        .and_then(|event| event["data"]["message_id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("no InboxEnqueued carries {needle:?}: {events:#?}"))
}

/// A newline-delimited JSON journal, as `radian exec` writes it.
fn journal(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("the journal exists")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The `_meta.radian` text and cost `radian acp` folds from a journal, so a test can compare
/// the answer against the final file rather than against a literal.
fn journal_summary(events: &[Value]) -> (String, Value) {
    let mut text = String::new();
    let mut micros: Option<u64> = None;
    let mut incomplete = false;
    for event in events {
        match event["event"].as_str() {
            Some("MessageAppended") => {
                let Some(content) = event["data"]["message"]["assistant"]["content"].as_array()
                else {
                    continue;
                };
                let message: String = content
                    .iter()
                    .filter_map(|part| part["text"]["text"].as_str())
                    .collect();
                if !message.is_empty() {
                    text = message;
                }
            }
            Some("ModelUsageRecorded" | "CompactionUsageRecorded") => {
                let fee = &event["data"]["fee"];
                if let Some(value) = fee["micros"].as_u64() {
                    micros = Some(micros.unwrap_or(0).saturating_add(value));
                    incomplete |= fee["incomplete"].as_bool() == Some(true);
                }
            }
            _ => {}
        }
    }
    let cost = match micros {
        Some(micros) => json!({
            "micros": micros,
            "incomplete": incomplete,
            "estimated": false,
        }),
        None => Value::Null,
    };
    (text, cost)
}

/// The fee and last answer text the journal records for **one turn** (between its
/// `TurnStarted` and `TurnFinished`), so a test can compare a prompt answer against its own
/// turn rather than the session.
fn turn_summary(events: &[Value], turn_id: &str) -> (String, Option<u64>, bool) {
    let mut text = String::new();
    let mut micros: Option<u64> = None;
    let mut incomplete = false;
    let mut inside = false;
    for event in events {
        let data = &event["data"];
        match event["event"].as_str() {
            Some("TurnStarted") if data["turn_id"] == turn_id => inside = true,
            Some("TurnFinished") if data["turn_id"] == turn_id => break,
            Some("MessageAppended") if inside => {
                let Some(content) = data["message"]["assistant"]["content"].as_array() else {
                    continue;
                };
                let message: String = content
                    .iter()
                    .filter_map(|part| part["text"]["text"].as_str())
                    .collect();
                if !message.is_empty() {
                    text = message;
                }
            }
            Some("ModelUsageRecorded" | "CompactionUsageRecorded") if inside => {
                let fee = &data["fee"];
                if let Some(value) = fee["micros"].as_u64() {
                    micros = Some(micros.unwrap_or(0).saturating_add(value));
                    incomplete |= fee["incomplete"].as_bool() == Some(true);
                }
            }
            _ => {}
        }
    }
    (text, micros, incomplete)
}

/// The top-level `seq` of the first journal line the predicate accepts.
fn line_seq(events: &[Value], what: &str, accept: impl Fn(&Value) -> bool) -> u64 {
    events
        .iter()
        .find(|event| accept(event))
        .and_then(|event| event["seq"].as_u64())
        .unwrap_or_else(|| panic!("the journal never held {what}: {events:#?}"))
}

/// A text reply that also reports one call's usage, so the writer records a fee for the turn
/// (the catalog cache the test writes supplies the prices).
fn priced_text_reply(text: &str, prompt: u64, completion: u64) -> Reply {
    Reply::sse(&[
        chunk(json!({"content": text}), None),
        chunk(json!({}), Some("stop")),
        json!({
            "choices": [],
            "usage": {"prompt_tokens": prompt, "completion_tokens": completion},
        }),
    ])
}

/// Writes the catalog cache a process prices its calls from (ADR 0018/0027): model `m` under
/// `openai-compatible` (the provider `--base-url` resolves to), $3/M input and $15/M output.
fn write_priced_catalog(home: &Path) {
    let path = home.join("cache/catalog/models.json");
    std::fs::create_dir_all(path.parent().expect("the cache directory"))
        .expect("the catalog cache directory");
    std::fs::write(
        &path,
        json!({
            "openai-compatible": {
                "models": {
                    "m": {
                        "limit": {"context": 200_000, "output": 8_192},
                        "cost": {"input": 3.0, "output": 15.0},
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("the catalog cache");
}

/// ADR 0049: the answer follows the driver's journaled `TurnFinished`, not the core's live
/// `RunFinished`. The driver's commit-delay hook widens that gap: an answer on the live event
/// arrives while the journal still lacks the turn end, and its `_meta` is folded too early.
#[tokio::test]
async fn the_prompt_answer_waits_for_the_journaled_turn_end() {
    let server = MockServer::start(vec![text_reply("durable answer")]).await;
    let env = Env::fresh("durable-turn");
    let mut acp = Acp::spawn_with(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
        // A gap wide enough that the response's own latency cannot close it.
        &[("RADIAN_TURN_COMMIT_DELAY_MS", "2000")],
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp.prompt(&session, "say it", Decision::None).await;
    let result = run.result();
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    // Read immediately after the answer: with the delayed commit, a response that followed
    // `RunFinished` would find no turn end here.
    let events = journal(&env.sessions.join(format!("{session}.jsonl")));
    assert!(
        events.iter().any(|event| event["event"] == "TurnFinished"),
        "the answer arrived before the turn was durable: {events:?}"
    );
    let (text, cost) = journal_summary(&events);
    assert_eq!(result["_meta"]["radian"]["text"], text, "{result}");
    assert_eq!(result["_meta"]["radian"]["cost"], cost, "{result}");
    assert_eq!(
        text, "durable answer",
        "the turn's answer is in the journal"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn a_prompt_streams_updates_and_answers_with_cost_and_text() {
    let server = MockServer::start(vec![
        tool_reply("call-1", "read", json!({"path": "hello.txt"})),
        text_reply("done"),
    ])
    .await;
    let env = Env::fresh("main");
    std::fs::write(env.workspace.join("hello.txt"), "hello").unwrap();
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
    );
    let init = acp.initialize().await;
    assert_eq!(
        init["result"]["agentCapabilities"]["loadSession"],
        json!(true),
        "session/load is advertised"
    );
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp.prompt(&session, "read hello", Decision::None).await;
    let result = run.result();
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    assert_eq!(result["_meta"]["radian"]["text"], "done", "{result}");
    assert!(
        result["_meta"]["radian"].get("cost").is_some(),
        "_meta.radian.cost is present (null without a catalog price): {result}"
    );
    assert!(result["_meta"]["radian"]["cost"].is_null(), "{result}");
    assert!(
        run.updates.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk" && update["content"]["text"] == "done"
        }),
        "an agent_message_chunk carries the answer: {:?}",
        run.updates
    );
    let tool = run
        .updates
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call")
        .expect("a tool_call");
    assert_eq!(tool["toolCallId"], "call-1");
    assert_eq!(tool["kind"], "read");
    assert_eq!(tool["status"], "in_progress");
    assert_eq!(tool["rawInput"]["path"], "hello.txt");
    assert!(
        tool["title"]
            .as_str()
            .is_some_and(|title| title.contains("hello.txt")),
        "{tool}"
    );
    assert!(
        run.updates.iter().any(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call-1"
                && update["status"] == "completed"
        }),
        "a tool_call_update finishes the call: {:?}",
        run.updates
    );
    let path = env.sessions.join(format!("{session}.jsonl"));
    assert!(path.exists(), "the journal exists at {path:?}");
    let events = journal(&path);
    assert!(
        events.iter().any(|event| event["event"] == "TurnFinished"),
        "the turn is durable"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0017 §2 through ADR 0049: a truncated answer continues the turn with a notice, and the
/// third truncated answer in a row answers `max_tokens`.
#[tokio::test]
async fn max_tokens_answers_after_two_continuations() {
    let truncated = || {
        Reply::sse(&[
            chunk(json!({"content": "partial"}), None),
            chunk(json!({}), Some("length")),
        ])
    };
    let server = MockServer::start(vec![truncated(), truncated(), truncated()]).await;
    let env = Env::fresh("max-tokens");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", &server.base_url], &env);
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();

    let run = acp.prompt(&session, "hi", Decision::None).await;
    let result = run.result();

    assert_eq!(result["stopReason"], "max_tokens", "{result}");
    assert_eq!(server.requests().len(), 3, "one model call per truncation");
    // The continuation request carries the durable notice after the truncated answer.
    let second = server.requests()[1].json();
    let messages = second["messages"].as_array().expect("messages");
    assert_eq!(messages[messages.len() - 2]["role"], json!("assistant"));
    assert_eq!(
        messages.last().expect("a message"),
        &json!({
            "role": "user",
            "content": "Your last response hit the output token limit. Do not apologize, recap, or repeat yourself. Continue from where you stopped, and break the remaining work into smaller pieces.",
        })
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn a_rejected_permission_reaches_the_tool_result() {
    let server = MockServer::start(vec![
        tool_reply(
            "call-shell",
            "shell",
            json!({
                "command": "echo hi",
                "outside_sandbox": true,
                "outside_sandbox_reason": "the ACP approval test",
            }),
        ),
        text_reply("after rejection"),
    ])
    .await;
    let env = Env::fresh("approval");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &env,
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp.prompt(&session, "run it", Decision::Reject).await;
    assert_eq!(run.result()["stopReason"], "end_turn");
    assert_eq!(run.permissions.len(), 1, "{:?}", run.permissions);
    let permission = &run.permissions[0];
    assert_eq!(permission["toolCall"]["toolCallId"], "call-shell");
    let kinds: Vec<&str> = permission["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(|option| option["kind"].as_str())
        .collect();
    for kind in ["allow_once", "allow_always", "reject_once"] {
        assert!(kinds.contains(&kind), "the options carry {kind}: {kinds:?}");
    }
    let failed = run
        .updates
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update"
                && update["toolCallId"] == "call-shell"
                && update["rawOutput"].is_object()
        })
        .expect("the tool result update");
    assert_eq!(failed["status"], "failed", "{failed}");
    assert!(
        failed["rawOutput"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("the user rejected this call")),
        "the tool result carries the reason: {failed}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn an_ask_user_question_becomes_a_permission_request_and_the_answer_reaches_the_tool() {
    let server = MockServer::start(vec![
        tool_reply(
            "call-ask",
            "ask_user",
            json!({
                "questions": [{
                    "id": "tier",
                    "prompt": "Which tier?",
                    "kind": "single",
                    "options": [
                        { "id": "a", "label": "A" },
                        { "id": "b", "label": "B" },
                    ],
                    "required": true,
                }],
            }),
        ),
        text_reply("answered"),
    ])
    .await;
    let env = Env::fresh("ask-user");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", &server.base_url], &env);
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp
        .prompt(&session, "ask me", Decision::Select("tier:b"))
        .await;
    assert_eq!(run.result()["stopReason"], "end_turn");
    assert_eq!(run.permissions.len(), 1, "{:?}", run.permissions);
    let permission = &run.permissions[0];
    assert_eq!(permission["toolCall"]["title"], "Which tier?");
    let options: Vec<&str> = permission["options"]
        .as_array()
        .expect("options")
        .iter()
        .filter_map(|option| option["optionId"].as_str())
        .collect();
    assert!(options.contains(&"tier:a"), "{options:?}");
    assert!(options.contains(&"tier:b"), "{options:?}");
    // The answer is the tool result, and the next model request carries it.
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "the turn asked the model again");
    assert!(
        requests[1].body.contains("Which tier? → B"),
        "the question's answer reaches the model: {}",
        requests[1].body
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn a_cancelled_turn_answers_cancelled_and_keeps_the_interrupted_prefix() {
    let pieces = vec![
        format!(
            "data: {}\n\n",
            chunk(json!({"content": "partial answer"}), None)
        )
        .into_bytes(),
    ];
    let server = MockServer::start(vec![Reply::SseThenHang { pieces }]).await;
    let env = Env::fresh("cancel");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", &server.base_url], &env);
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp.prompt(&session, "talk", Decision::CancelOnText).await;
    assert_eq!(run.result()["stopReason"], "cancelled", "{}", run.response);
    let events = journal(&env.sessions.join(format!("{session}.jsonl")));
    let interrupted = events
        .iter()
        .find(|event| {
            event["event"] == "MessageAppended"
                && event["data"]["message"]["assistant"]["interrupted"] == json!(true)
        })
        .expect("the interrupted assistant prefix is journaled");
    let text = interrupted["data"]["message"]["assistant"]["content"][0]["text"]["text"]
        .as_str()
        .expect("the prefix text");
    assert!(text.contains("partial answer"), "{interrupted}");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// A failed `session/new` consumes nothing, so the client can fix the cause and retry in the
/// same process (ADR 0049). Mutation: take the child headers before `setup` succeeds → the
/// retry answers the internal "already consumed" error instead of opening the session.
#[tokio::test]
async fn a_failed_session_new_can_be_retried() {
    let env = Env::fresh("retry");
    let config = env.home.join("config");
    std::fs::create_dir_all(&config).unwrap();
    let file = config.join("config.toml");
    std::fs::write(
        &file,
        "schema_version = 1\n[permissions]\napproval = \"sometimes\"\n",
    )
    .unwrap();
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.initialize().await;
    let failed = acp.new_session(&env.workspace).await;
    let message = failed["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("permissions.approval"),
        "the refused config is reported: {failed}"
    );
    std::fs::write(&file, "schema_version = 1\n").unwrap();
    // A different cwd proves the failed attempt left no session state behind either: the
    // workspace it was refused for does not become this process's one workspace.
    let other = env.home.join("ws2");
    std::fs::create_dir_all(&other).unwrap();
    let retried = acp.new_session(&other).await;
    assert!(
        retried.get("error").is_none(),
        "the fixed config opens the session: {retried}"
    );
    assert!(
        retried["result"]["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "the retry answers a session id: {retried}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn one_process_holds_one_session() {
    let env = Env::fresh("one-session");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.initialize().await;
    let first = acp.new_session(&env.workspace).await;
    assert!(first.get("error").is_none(), "{first}");
    let second = acp.new_session(&env.workspace).await;
    let message = second["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("one process holds one session"),
        "the second session/new is refused: {second}"
    );
    let other = env.home.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let third = acp.new_session(&other).await;
    let message = third["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("one process holds one session"),
        "a differing cwd is refused: {third}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn a_model_error_is_a_refusal_with_the_error() {
    let server = MockServer::start(vec![Reply::status(400, "provider is down")]).await;
    let env = Env::fresh("model-error");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", &server.base_url], &env);
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let run = acp.prompt(&session, "hi", Decision::None).await;
    let result = run.result();
    assert_eq!(result["stopReason"], "refusal", "{result}");
    let error = result["_meta"]["radian"]["error"]
        .as_str()
        .expect("_meta.radian.error");
    assert!(error.contains("HTTP 400"), "{error}");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn http_mcp_servers_are_refused() {
    let env = Env::fresh("mcp");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.initialize().await;
    let new = acp
        .request(
            "session/new",
            json!({
                "cwd": env.workspace,
                "mcpServers": [{
                    "type": "http",
                    "name": "remote",
                    "url": "https://example.test/mcp",
                    "headers": [],
                }],
            }),
        )
        .await;
    let message = new["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("http") && message.contains("not supported"),
        "an HTTP MCP server is refused by transport: {new}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// The acceptance test: one process writes a session, a second one loads it, the client gets
/// the earlier messages and tool calls as `session/update`s, and a prompt continues the **same**
/// journal (ADR 0006, ADR 0022: no fork). The journal then replays cleanly.
#[tokio::test]
async fn session_load_replays_the_journal_and_continues_it() {
    let server = MockServer::start(vec![
        tool_reply("call-1", "read", json!({ "path": "hello.txt" })),
        text_reply("first answer"),
        text_reply("second answer"),
    ])
    .await;
    let env = Env::fresh("load");
    std::fs::write(env.workspace.join("hello.txt"), "hello").unwrap();
    let args = [
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        env.workspace.to_str().unwrap(),
    ];
    let session = {
        let mut acp = Acp::spawn(&args, &env);
        acp.initialize().await;
        let new = acp.new_session(&env.workspace).await;
        let session = new["result"]["sessionId"]
            .as_str()
            .expect("a session id")
            .to_owned();
        let run = acp.prompt(&session, "read hello", Decision::None).await;
        assert_eq!(run.result()["stopReason"], "end_turn");
        let (code, stderr) = acp.finish().await;
        assert_eq!(code, 0, "{stderr}");
        session
    };

    let mut acp = Acp::spawn(&args, &env);
    acp.initialize().await;
    let (response, replayed) = acp.load_session(&session, &env.workspace).await;
    assert!(response.get("error").is_none(), "{response}");
    assert!(!replayed.is_empty(), "the transcript is replayed");
    assert_eq!(
        replayed[0]["sessionUpdate"], "user_message_chunk",
        "the replay starts with the earlier user message: {replayed:?}"
    );
    assert_eq!(replayed[0]["content"]["text"], "read hello");
    assert!(
        replayed.iter().any(|update| {
            update["sessionUpdate"] == "agent_message_chunk"
                && update["content"]["text"] == "first answer"
        }),
        "the earlier answer is replayed: {replayed:?}"
    );
    let call = replayed
        .iter()
        .find(|update| update["sessionUpdate"] == "tool_call")
        .expect("the earlier tool call is replayed");
    assert_eq!(call["toolCallId"], "call-1");
    assert_eq!(call["kind"], "read");
    assert_eq!(call["rawInput"]["path"], "hello.txt");
    let finished = replayed
        .iter()
        .find(|update| {
            update["sessionUpdate"] == "tool_call_update" && update["toolCallId"] == "call-1"
        })
        .expect("the earlier tool result is replayed");
    assert_eq!(finished["status"], "completed", "{finished}");
    assert!(
        finished["rawOutput"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("hello")),
        "the replayed result carries the output: {finished}"
    );
    // The replay is in transcript order: the message before the call it asked for, the call
    // before its result.
    let position = |predicate: &dyn Fn(&Value) -> bool| {
        replayed
            .iter()
            .position(predicate)
            .expect("the update is present")
    };
    assert!(
        position(&|update| update["sessionUpdate"] == "user_message_chunk")
            < position(&|update| update["sessionUpdate"] == "tool_call")
            && position(&|update| update["sessionUpdate"] == "tool_call")
                < position(&|update| update["sessionUpdate"] == "tool_call_update")
    );

    // A prompt on the loaded session continues the same journal.
    let run = acp.prompt(&session, "what next", Decision::None).await;
    assert_eq!(run.result()["stopReason"], "end_turn");
    assert_eq!(run.result()["_meta"]["radian"]["text"], "second answer");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");

    // The model's second request carries the earlier turn, so the journal continued rather
    // than starting a new one.
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "one call per turn");
    for text in ["read hello", "first answer", "what next"] {
        assert!(
            requests[2].body.contains(text),
            "the continued request lacks {text:?}: {}",
            requests[2].body
        );
    }

    let path = env.sessions.join(format!("{session}.jsonl"));
    let events = journal(&path);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "SessionCreated")
            .count(),
        1,
        "session/load must not start a second journal"
    );
    // The `journal-selfcheck` replay of the loaded session: the journal still folds to one
    // transcript, the earlier turn first and the new turn after it.
    let loaded = radian_session::Session::open(&path).expect("the loaded journal parses");
    let history = loaded
        .history()
        .expect("the loaded journal's selfcheck replay");
    let texts: Vec<String> = history.iter().map(radian_core::Message::text).collect();
    assert_eq!(
        texts.first().map(String::as_str),
        Some("read hello"),
        "{texts:?}"
    );
    assert!(texts.contains(&"first answer".to_owned()), "{texts:?}");
    assert!(texts.contains(&"what next".to_owned()), "{texts:?}");
    assert!(texts.contains(&"second answer".to_owned()), "{texts:?}");
}

#[tokio::test]
async fn session_load_of_an_unknown_id_is_an_error_that_names_it() {
    let env = Env::fresh("load-unknown");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let (response, replayed) = acp.load_session("missing-id", &env.workspace).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("missing-id"),
        "the error names the id: {response}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// A minimal, valid v1 session file for `workspace`, so a load can name a session without a
/// run: one `SessionCreated`, one user message and one assistant reply.
fn write_session(dir: &Path, id: &str, workspace: &str, user_text: &str) -> PathBuf {
    let ts = "2026-10-13T00:00:00.000000Z";
    let lines = [
        json!({"schema_version":1,"session_id":id,"seq":0,"ts":ts,"event":"SessionCreated",
               "critical":true,"data":{"workspace":workspace,"config_snapshot":{}}}),
        json!({"schema_version":1,"session_id":id,"seq":1,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":0,
               "message":{"user":{"content":[{"text":{"text":user_text}}]}}}}),
        json!({"schema_version":1,"session_id":id,"seq":2,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":1,
               "message":{"assistant":{"content":[{"text":{"text":"reply"}}]}}}}),
    ];
    let mut text = String::new();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    let path = dir.join(format!("{id}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

/// The canonical form `resolve_workspace` records, so a hand-written `SessionCreated` matches
/// what a real run would write (macOS `/var` -> `/private/var` included).
fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("the directory exists")
        .to_string_lossy()
        .into_owned()
}

/// Two workspaces and a session created in the first, for the workspace rule of
/// `session/load` (ADR 0049 decision 11).
fn two_workspaces(prefix: &str) -> (Env, PathBuf, PathBuf, PathBuf) {
    let env = Env::fresh(prefix);
    let ws_a = env.home.join("ws-a");
    let ws_b = env.home.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let session = write_session(&env.sessions, "from-a", &canonical(&ws_a), "hello from a");
    (env, ws_a, ws_b, session)
}

/// ADR 0049 decision 11: a session created in workspace A is refused in a process whose
/// workspace is B: the error names it and says it is not in this workspace, and the journal is
/// not written.
#[tokio::test]
async fn session_load_refuses_a_session_of_another_workspace() {
    let (env, _ws_a, ws_b, session) = two_workspaces("load-other-ws");
    let before = std::fs::read(&session).unwrap();
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            ws_b.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let (response, replayed) = acp.load_session("from-a", &ws_b).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("from-a"),
        "the error names the session: {response}"
    );
    assert!(
        message.contains("not in this workspace"),
        "the error states the workspace rule: {response}"
    );
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "a refused load writes nothing to the journal"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 11: without `--workspace` the request's `cwd` is the process workspace,
/// and the same refusal applies to a session created elsewhere.
#[tokio::test]
async fn session_load_refuses_another_workspace_when_the_cwd_sets_it() {
    let (env, _ws_a, ws_b, _session) = two_workspaces("load-other-ws-cwd");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.initialize().await;
    let (response, replayed) = acp.load_session("from-a", &ws_b).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    assert_eq!(response["error"]["code"], json!(-32602), "{response}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("from-a") && message.contains("not in this workspace"),
        "the error names the session and the rule: {response}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 11: both the recorded and the process workspace are canonical
/// (`resolve_workspace`), so the same directory reached through a symlink is accepted. The
/// other workspace in the same process is refused first, so the acceptance of the linked
/// directory is the changed behavior, not the old "load any named session".
#[cfg(unix)]
#[tokio::test]
async fn session_load_accepts_the_workspace_reached_through_a_symlink() {
    let env = Env::fresh("load-symlink");
    let ws_a = env.home.join("ws-a");
    let ws_b = env.home.join("ws-b");
    let link = env.home.join("ws-link");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    std::os::unix::fs::symlink(&ws_a, &link).unwrap();
    write_session(&env.sessions, "in-a", &canonical(&ws_a), "hello from a");
    write_session(&env.sessions, "in-b", &canonical(&ws_b), "hello from b");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            link.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    // The process workspace is the link's target (`ws-a`), so `in-b` is another workspace and
    // is refused. This is what the pre-change code did not do: it loaded every named session.
    let (refused, replayed) = acp.load_session("in-b", &link).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    assert_eq!(refused["error"]["code"], json!(-32602), "{refused}");
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("in-b") && message.contains("not in this workspace"),
        "the error names the session and the rule: {refused}"
    );
    // The link still matches the canonical workspace `in-a` recorded.
    let (response, replayed) = acp.load_session("in-a", &link).await;
    assert!(response.get("error").is_none(), "{response}");
    assert!(
        replayed.iter().any(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["text"] == "hello from a"
        }),
        "the session's transcript is replayed: {replayed:?}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 11: a refused load consumes nothing, so the process can still load a
/// session that belongs to its workspace.
#[tokio::test]
async fn a_refused_load_leaves_the_process_able_to_load_a_valid_session() {
    let (env, _ws_a, ws_b, _other) = two_workspaces("load-then-valid");
    write_session(&env.sessions, "in-b", &canonical(&ws_b), "hello from b");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            ws_b.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let (refused, replayed) = acp.load_session("from-a", &ws_b).await;
    assert!(refused.get("error").is_some(), "{refused}");
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    let (loaded, replayed) = acp.load_session("in-b", &ws_b).await;
    assert!(loaded.get("error").is_none(), "{loaded}");
    assert!(
        replayed.iter().any(|update| {
            update["sessionUpdate"] == "user_message_chunk"
                && update["content"]["text"] == "hello from b"
        }),
        "the valid session is replayed: {replayed:?}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 11: a refused load consumes nothing, so the process can still start its
/// own session in the process workspace.
#[tokio::test]
async fn a_refused_load_leaves_the_process_able_to_start_a_new_session() {
    let (env, _ws_a, ws_b, _other) = two_workspaces("load-then-new");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            ws_b.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let (refused, replayed) = acp.load_session("from-a", &ws_b).await;
    assert!(refused.get("error").is_some(), "{refused}");
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    let new = acp.new_session(&ws_b).await;
    assert!(new.get("error").is_none(), "{new}");
    assert!(
        new["result"]["sessionId"].as_str().is_some(),
        "a session is opened after the refusal: {new}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 11: a journal whose `SessionCreated` cannot be read is the opener's error
/// path, not this refusal and not a panic. The refusal of a valid other-workspace session comes
/// first, so the test shows the two errors are different; the pre-change code accepted that
/// first load (no refusal exists), so this fails on the old behavior.
#[tokio::test]
async fn session_load_of_a_truncated_journal_is_the_openers_error() {
    let (env, _ws_a, ws_b, _other) = two_workspaces("load-truncated");
    std::fs::write(
        env.sessions.join("broken.jsonl"),
        "{\"schema_version\":1,\"session_id\":\"broken\",\"seq\":0,\"event\":\"SessionCreated\",\n",
    )
    .unwrap();
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:1",
            "--workspace",
            ws_b.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let (refused, replayed) = acp.load_session("from-a", &ws_b).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    assert_eq!(refused["error"]["code"], json!(-32602), "{refused}");
    let (response, replayed) = acp.load_session("broken", &ws_b).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    assert_eq!(response["error"]["code"], json!(-32603), "{response}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("cannot resume"),
        "the opener's error path, not the workspace refusal: {response}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn session_load_after_session_new_is_refused() {
    let env = Env::fresh("load-after-new");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let (response, replayed) = acp.load_session(&session, &env.workspace).await;
    assert!(replayed.is_empty(), "nothing is replayed: {replayed:?}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("one process holds one session"),
        "a second session is refused: {response}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049: `allow_always` is a grant for the process that answered it. A `session/load` in a
/// new process restores none.
#[tokio::test]
async fn a_session_grant_is_not_restored_by_session_load() {
    let call = || {
        tool_reply(
            "call-shell",
            "shell",
            json!({
                "command": "echo hi",
                "outside_sandbox": true,
                "outside_sandbox_reason": "the ACP grant test",
            }),
        )
    };
    let server = MockServer::start(vec![
        call(),
        text_reply("ran"),
        call(),
        text_reply("ran again"),
    ])
    .await;
    let env = Env::fresh("load-grant");
    let args = [
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        env.workspace.to_str().unwrap(),
        "--profile",
        "workspace",
    ];
    let session = {
        let mut acp = Acp::spawn(&args, &env);
        acp.initialize().await;
        let new = acp.new_session(&env.workspace).await;
        let session = new["result"]["sessionId"]
            .as_str()
            .expect("a session id")
            .to_owned();
        let run = acp
            .prompt(&session, "run it", Decision::Select("allow_always"))
            .await;
        assert_eq!(run.result()["stopReason"], "end_turn");
        assert_eq!(run.permissions.len(), 1, "{:?}", run.permissions);
        let (code, stderr) = acp.finish().await;
        assert_eq!(code, 0, "{stderr}");
        session
    };

    let mut acp = Acp::spawn(&args, &env);
    acp.initialize().await;
    let (response, _) = acp.load_session(&session, &env.workspace).await;
    assert!(response.get("error").is_none(), "{response}");
    let run = acp.prompt(&session, "run it again", Decision::Reject).await;
    assert_eq!(run.result()["stopReason"], "end_turn");
    assert_eq!(
        run.permissions.len(),
        1,
        "the earlier allow_always is gone, so the call is asked again: {:?}",
        run.permissions
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

#[tokio::test]
async fn acp_help_has_the_conversation_flags_but_not_the_exec_only_ones() {
    let env = Env::fresh("help");
    let mut acp = Acp::spawn(&["--help"], &env);
    // `--help` exits without a protocol conversation; stdout is the help text.
    let mut out = String::new();
    while let Some(line) = timeout(EXIT, acp.stdout.next_line())
        .await
        .expect("help timed out")
        .expect("read help")
    {
        out.push_str(&line);
        out.push('\n');
    }
    for flag in [
        "--workspace",
        "--profile",
        "--resume",
        "--session-id",
        "--system-file",
        "--mcp-config",
    ] {
        assert!(
            out.contains(flag),
            "radian acp --help misses {flag}:\n{out}"
        );
    }
    for flag in ["--yes", "--output", "--prompt", "--image"] {
        assert!(
            !out.contains(flag),
            "radian acp --help must not offer {flag}:\n{out}"
        );
    }
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 4: a prompt is answered by the turn that claimed **its** message. A
/// background job from the earlier prompt settles between this prompt entering the inbox and
/// the driver taking it, so the job's completion notice opens a system wake turn first (ADR
/// 0037); that turn's end answers nothing, and its text and status are not reported.
///
/// The claim-hold file makes the order deterministic: while it exists the driver claims
/// nothing, so the test queues the notice, then the prompt, and only then lets the loop run.
#[tokio::test]
async fn a_wake_turn_end_does_not_answer_the_prompt_after_it() {
    let server = MockServer::start(vec![
        bg_reply("call-1", "while [ ! -f go ]; do sleep 0.05; done"),
        text_reply("PROMPT-ONE"),
        // The wake turn fails: its end is a `refusal` with an error, so an answer taken from it
        // shows in both `stopReason` and `_meta.radian.error`.
        Reply::status(400, "the wake turn failed"),
        text_reply("PROMPT-TWO"),
    ])
    .await;
    let env = Env::fresh("wake-turn");
    let hold = env.home.join("claim-hold");
    let hold_path = hold.to_str().unwrap().to_owned();
    let mut acp = Acp::spawn_with(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
            "--approval",
            "auto",
        ],
        &env,
        &[("RADIAN_CLAIM_HOLD_FILE", &hold_path)],
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let path = env.sessions.join(format!("{session}.jsonl"));

    let first = acp.prompt(&session, "start the job", Decision::None).await;
    assert_eq!(
        first.result()["_meta"]["radian"]["text"],
        "PROMPT-ONE",
        "{}",
        first.response
    );

    // The first turn is over and the driver claims nothing now: hold the claim, settle the job
    // on the test's signal, and queue the second prompt behind the notice.
    std::fs::write(&hold, "").unwrap();
    std::fs::write(env.workspace.join("go"), "").unwrap();
    wait_for_event(
        &path,
        "the job's completion notice",
        "InboxEnqueued",
        |event| {
            event["data"]["message"]
                .to_string()
                .contains("background job")
        },
    )
    .await;
    let id = acp.send_prompt(&session, "second question").await;
    wait_for_event(
        &path,
        "the second prompt's enqueue",
        "InboxEnqueued",
        |event| {
            event["data"]["message"]
                .to_string()
                .contains("second question")
        },
    )
    .await;
    std::fs::remove_file(&hold).unwrap();

    let second = acp.collect_prompt(id, &session, Decision::None).await;
    let result = second.result();
    assert_eq!(
        result["stopReason"], "end_turn",
        "the wake turn's failed end answered the prompt: {result}"
    );
    assert!(
        result["_meta"]["radian"].get("error").is_none(),
        "the wake turn's error reached the prompt's answer: {result}"
    );
    assert_eq!(
        result["_meta"]["radian"]["text"], "PROMPT-TWO",
        "the wake turn's end answered the prompt: {result}"
    );

    // The journal agrees: three turns ran, the last one claimed the prompt's message, and the
    // prompt's own turn is the one that ended last.
    let events = journal(&path);
    let finished: Vec<&str> = events
        .iter()
        .filter(|event| event["event"] == "TurnFinished")
        .filter_map(|event| event["data"]["turn_id"].as_str())
        .collect();
    assert_eq!(
        finished.len(),
        3,
        "a wake turn ran before the prompt's: {events:#?}"
    );
    let prompt_id = enqueued_id(&events, "second question");
    // The hold put the prompt in the inbox **before** the driver claimed the notice — the
    // defect's precondition. Without the hold the notice is claimed at once, the wake turn runs
    // before the prompt exists, and the test would pass without exercising the defect.
    let notice_id = enqueued_id(&events, "background job");
    let prompt_enqueue = line_seq(&events, "the prompt's enqueue", |event| {
        event["event"] == "InboxEnqueued"
            && event["data"]["message_id"].as_str() == Some(prompt_id.as_str())
    });
    let notice_claim = line_seq(&events, "the notice's claim", |event| {
        event["event"] == "InputClaimed"
            && event["data"]["message_ids"]
                .to_string()
                .contains(&notice_id)
    });
    assert!(
        prompt_enqueue < notice_claim,
        "the prompt must be enqueued before the driver claims the notice: {events:#?}"
    );
    let claimed_turn = events
        .iter()
        .find(|event| {
            event["event"] == "InputClaimed"
                && event["data"]["message_ids"]
                    .to_string()
                    .contains(&prompt_id)
        })
        .and_then(|event| event["data"]["turn_id"].as_str())
        .unwrap_or_else(|| panic!("the prompt's message was never claimed: {events:#?}"));
    assert_eq!(
        Some(claimed_turn),
        finished.last().copied(),
        "the prompt is answered by its own turn: {events:#?}"
    );
    assert_eq!(
        server.requests().len(),
        4,
        "one model call per turn, three turns"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decisions 4 and 6: the answer's `_meta.radian` is the turn that answered the
/// prompt, not the session. A job from the first prompt settles between the second prompt's
/// enqueue and its claim; the notice's wake turn records its own priced call, so an answer that
/// folded the session would report the first turn's and the wake turn's fees too.
#[tokio::test]
async fn a_prompt_reports_its_own_turns_cost_and_text_not_the_sessions() {
    let server = MockServer::start(vec![
        bg_reply("call-1", "while [ ! -f go ]; do sleep 0.05; done"),
        priced_text_reply("PROMPT-ONE", 100, 10),
        priced_text_reply("WAKE-ANSWER", 200, 20),
        priced_text_reply("PROMPT-TWO", 300, 30),
    ])
    .await;
    let env = Env::fresh("prompt-cost");
    write_priced_catalog(&env.home);
    let hold = env.home.join("claim-hold");
    let hold_path = hold.to_str().unwrap().to_owned();
    let mut acp = Acp::spawn_with(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
            "--approval",
            "auto",
        ],
        &env,
        &[("RADIAN_CLAIM_HOLD_FILE", &hold_path)],
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let path = env.sessions.join(format!("{session}.jsonl"));

    let first = acp.prompt(&session, "start the job", Decision::None).await;
    assert_eq!(
        first.result()["_meta"]["radian"]["text"],
        "PROMPT-ONE",
        "{}",
        first.response
    );
    let first_fee = first.result()["_meta"]["radian"]["cost"]["micros"]
        .as_u64()
        .expect("the priced catalog gives the first turn a fee");
    assert_eq!(
        Some(first_fee),
        turn_summary(&journal(&path), "t-1").1,
        "the first answer reports its own turn's fee"
    );

    std::fs::write(&hold, "").unwrap();
    std::fs::write(env.workspace.join("go"), "").unwrap();
    wait_for_event(
        &path,
        "the job's completion notice",
        "InboxEnqueued",
        |event| {
            event["data"]["message"]
                .to_string()
                .contains("background job")
        },
    )
    .await;
    let id = acp.send_prompt(&session, "second question").await;
    wait_for_event(
        &path,
        "the second prompt's enqueue",
        "InboxEnqueued",
        |event| {
            event["data"]["message"]
                .to_string()
                .contains("second question")
        },
    )
    .await;
    std::fs::remove_file(&hold).unwrap();

    let second = acp.collect_prompt(id, &session, Decision::None).await;
    let result = second.result();
    let events = journal(&path);
    let prompt_id = enqueued_id(&events, "second question");
    let claimed_turn = events
        .iter()
        .find(|event| {
            event["event"] == "InputClaimed"
                && event["data"]["message_ids"]
                    .to_string()
                    .contains(&prompt_id)
        })
        .and_then(|event| event["data"]["turn_id"].as_str())
        .unwrap_or_else(|| panic!("the prompt's message was never claimed: {events:#?}"));
    let (text, micros, incomplete) = turn_summary(&events, claimed_turn);
    assert_eq!(text, "PROMPT-TWO", "the prompt's own turn answered");
    assert_eq!(
        result["_meta"]["radian"]["text"], text,
        "the answer reports its own turn's text: {result}"
    );
    let cost = &result["_meta"]["radian"]["cost"];
    assert_eq!(
        cost["micros"].as_u64(),
        micros,
        "the answer reports its own turn's fee, not the session's: {result}"
    );
    assert_eq!(
        cost["incomplete"].as_bool(),
        Some(incomplete),
        "the answer reports its own turn's completeness: {result}"
    );
    assert_eq!(
        cost["estimated"].as_bool(),
        Some(false),
        "a live turn's fee is priced as it arrives, never an estimate: {result}"
    );
    let (_, session_cost) = journal_summary(&events);
    assert_ne!(
        session_cost["micros"], cost["micros"],
        "the first turn's and the wake turn's fees are not this answer's: {result}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 4: a prompt that arrives while a wake turn runs is refused, and the wake
/// turn's end never answers it. Once the wake turn is over, the prompt gets its own turn.
#[tokio::test]
async fn a_prompt_during_a_wake_turn_is_refused_and_answers_after_it() {
    let server = MockServer::start(vec![
        bg_reply("call-1", "while [ ! -f go ]; do sleep 0.05; done"),
        text_reply("PROMPT-ONE"),
        // The wake turn's model call never answers: the turn stays running.
        Reply::SseThenHang { pieces: Vec::new() },
        text_reply("PROMPT-TWO"),
    ])
    .await;
    let env = Env::fresh("wake-running");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
            "--approval",
            "auto",
        ],
        &env,
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let path = env.sessions.join(format!("{session}.jsonl"));

    let first = acp.prompt(&session, "start the job", Decision::None).await;
    assert_eq!(
        first.result()["_meta"]["radian"]["text"],
        "PROMPT-ONE",
        "{}",
        first.response
    );
    std::fs::write(env.workspace.join("go"), "").unwrap();
    wait_for_event(&path, "the wake turn", "TurnStarted", |event| {
        event["data"]["turn_id"] == "t-2"
    })
    .await;

    let refused = acp
        .prompt(&session, "second question", Decision::None)
        .await;
    let message = refused.response["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("a second session/prompt during a turn is refused"),
        "a prompt during a wake turn is refused, not answered by the wake turn: {message}"
    );

    // End the wake turn, then the prompt gets a turn of its own. The cancel must land while the
    // turn's model call is in flight: a cancel sent before the driver's turn starts is a no-op.
    wait_for_requests(&server, 3, "the wake turn's model call").await;
    acp.cancel(&session).await;
    wait_for_event(&path, "the wake turn's end", "TurnFinished", |event| {
        event["data"]["turn_id"] == "t-2" && event["data"]["outcome"] == "cancelled"
    })
    .await;
    let run = acp
        .prompt(&session, "second question", Decision::None)
        .await;
    let result = run.result();
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    assert_eq!(result["_meta"]["radian"]["text"], "PROMPT-TWO", "{result}");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049 decision 4: two prompts sent back to back without waiting are refused for the
/// second (the first turn is still running), and the first answers from its own turn — a
/// `cancelled` end is that turn's status, not a later turn's.
#[tokio::test]
async fn a_second_prompt_back_to_back_is_refused_and_the_first_answers_its_own_turn() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let env = Env::fresh("back-to-back");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();

    let first = acp.send_prompt(&session, "first").await;
    let second = acp.send_prompt(&session, "second").await;
    let refused = acp.collect_prompt(second, &session, Decision::None).await;
    let message = refused.response["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("a second session/prompt during a turn is refused"),
        "the second prompt is refused: {message}"
    );

    // Cancel once the turn is really running: a cancel that lands before the driver's turn
    // starts is a no-op (the turn's cancellation episode begins with the turn).
    wait_for_requests(&server, 1, "the first turn's model call").await;
    acp.cancel(&session).await;
    let run = acp.collect_prompt(first, &session, Decision::None).await;
    assert_eq!(run.result()["stopReason"], "cancelled", "{}", run.response);
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049: one input line is at most `ACP_MAX_INPUT_LINE_BYTES`. A longer line is discarded
/// as it is read and answered with a JSON-RPC parse error whose message names the limit, and
/// the next line is served normally. The old reader held the whole line and reported it as
/// invalid params instead.
#[tokio::test]
async fn an_over_long_line_is_a_parse_error_and_the_next_line_is_served() {
    let env = Env::fresh("long-line");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.write_filler(ACP_MAX_INPUT_LINE_BYTES + 1).await;
    acp.write_bytes(b"\n").await;

    let error = acp.read().await;
    assert_eq!(error["jsonrpc"], "2.0", "{error}");
    assert_eq!(
        error["id"],
        Value::Null,
        "a parse error has a null id: {error}"
    );
    assert_eq!(error["error"]["code"], json!(-32700), "{error}");
    let message = error["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(&ACP_MAX_INPUT_LINE_BYTES.to_string()),
        "the parse error names the limit: {message}"
    );

    let init = acp.initialize().await;
    assert!(init.get("error").is_none(), "{init}");
    assert_eq!(init["result"]["protocolVersion"], json!(1), "{init}");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049: a line exactly at the limit is accepted and parsed.
#[tokio::test]
async fn a_line_exactly_at_the_limit_is_accepted() {
    let env = Env::fresh("limit-line");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    let prefix =
        br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"protocolVersion":1},"pad":""#;
    let suffix = b"\"}";
    let filler = ACP_MAX_INPUT_LINE_BYTES - prefix.len() - suffix.len();
    acp.write_bytes(prefix).await;
    acp.write_filler(filler).await;
    acp.write_bytes(suffix).await;
    acp.write_bytes(b"\n").await;

    let response = acp.read().await;
    assert_eq!(response["id"], json!(7), "{response}");
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["protocolVersion"],
        json!(1),
        "{response}"
    );
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049: end of input in the middle of an over-long line is a normal end of input: the
/// process exits cleanly and writes no parse error for a line that never ended.
#[tokio::test]
async fn eof_in_the_middle_of_an_over_long_line_is_a_clean_shutdown() {
    let env = Env::fresh("long-eof");
    let mut acp = Acp::spawn(&["--model", "m", "--base-url", "http://127.0.0.1:1"], &env);
    acp.write_filler(ACP_MAX_INPUT_LINE_BYTES + 1024).await;
    let (code, stderr, messages) = acp.finish_reading().await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        messages.is_empty(),
        "an unterminated over-long line is not a parse error: {messages:?}"
    );
}

/// ADR 0049: the outgoing queue is bounded and never drops a message. A client that stops
/// reading stdout cannot make the turn finish (the old unbounded queue did), and when it reads
/// again every `session/update` arrives in order with nothing missing.
#[tokio::test]
async fn a_client_that_stops_reading_gets_every_update_in_order_when_it_reads_again() {
    // Far more than the writer's queue plus the stdout pipe holds.
    let texts: Vec<String> = (0..2000)
        .map(|index| format!("[{index:04}]{}", "x".repeat(4090)))
        .collect();
    let mut chunks: Vec<Value> = texts
        .iter()
        .map(|text| chunk(json!({ "content": text }), None))
        .collect();
    chunks.push(chunk(json!({}), Some("stop")));
    let server = MockServer::start(vec![Reply::sse(&chunks)]).await;
    let env = Env::fresh("backpressure");
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let path = env.sessions.join(format!("{session}.jsonl"));

    let id = acp.send_prompt(&session, "stream it").await;
    wait_for_requests(&server, 1, "the streaming turn's model call").await;
    // The client does not read stdout: the queue fills and the turn must wait. The old
    // unbounded queue let the turn finish while the client was not reading.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let events = journal(&path);
        assert!(
            !events.iter().any(|event| event["event"] == "TurnFinished"),
            "the turn finished while the client was not reading, so the output was not \
             backpressured: {events:#?}"
        );
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let run = acp.collect_prompt(id, &session, Decision::None).await;
    let result = run.result();
    assert_eq!(result["stopReason"], "end_turn", "{result}");
    let streamed: String = run
        .updates
        .iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect();
    let expected: String = texts.concat();
    assert_eq!(
        streamed, expected,
        "every update arrived in order and none was dropped"
    );
    assert_eq!(result["_meta"]["radian"]["text"], expected, "{result}");
    let (code, stderr) = acp.finish().await;
    assert_eq!(code, 0, "{stderr}");
}

/// A session whose first prompt streams far more text than the stdout pipe and the output
/// queue hold, with the client not reading stdout: the turn is stalled in a wait for room when
/// this returns. The `MockServer` is returned so the caller keeps it alive.
async fn stalled_stream(prefix: &str) -> (MockServer, Env, Acp, String) {
    let mut chunks: Vec<Value> = (0..2000)
        .map(|index| {
            chunk(
                json!({ "content": format!("[{index:04}]{}", "x".repeat(4090)) }),
                None,
            )
        })
        .collect();
    chunks.push(chunk(json!({}), Some("stop")));
    let server = MockServer::start(vec![Reply::sse(&chunks)]).await;
    let env = Env::fresh(prefix);
    let mut acp = Acp::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            env.workspace.to_str().unwrap(),
        ],
        &env,
    );
    acp.initialize().await;
    let new = acp.new_session(&env.workspace).await;
    let session = new["result"]["sessionId"]
        .as_str()
        .expect("a session id")
        .to_owned();
    let path = env.sessions.join(format!("{session}.jsonl"));
    acp.send_prompt(&session, "stream it").await;
    wait_for_requests(&server, 1, "the streaming turn's model call").await;
    // The client reads nothing: the queue fills, the sink waits, and no turn end is journaled.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let events = journal(&path);
        assert!(
            !events.iter().any(|event| event["event"] == "TurnFinished"),
            "the turn finished while the client was not reading: {events:#?}"
        );
        if std::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (server, env, acp, session)
}

/// ADR 0049: end of input while the output is backpressured is still a clean shutdown. The
/// client stops reading stdout mid-turn (the queue fills and the sink waits), then closes
/// stdin and keeps the pipe open. The process must exit instead of waiting for a client that
/// will not read: the pre-change code hung in `session.join` and this test timed out.
#[tokio::test]
async fn closing_stdin_while_output_is_backpressured_still_shuts_down() {
    let (_server, _env, acp, _session) = stalled_stream("backpressure-eof").await;
    let (code, stderr) = acp.exit_after_stdin_ends().await;
    assert_eq!(code, 0, "{stderr}");
}

/// ADR 0049: SIGTERM while the output is backpressured is the same shutdown: the signal must
/// not wait behind a producer that waits for a client that is not reading.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_while_output_is_backpressured_still_shuts_down() {
    let (_server, _env, acp, _session) = stalled_stream("backpressure-term").await;
    acp.signal(libc::SIGTERM);
    let (code, stderr) = acp.exit_while_input_stays_open().await;
    assert_eq!(code, 143, "{stderr}");
}

/// The manual's ACP limits come from the code, not from prose that can drift: the line limit,
/// the queue capacity and the parse-error code are the constants themselves.
#[test]
fn the_manual_names_the_acp_limits_from_the_code() {
    let text = radian_coding::docs::find("acp.md").expect("acp.md is in the table");
    let mib = ACP_MAX_INPUT_LINE_BYTES / (1024 * 1024);
    assert!(
        text.contains(&format!("{mib} MiB")),
        "acp.md names the {mib} MiB input line limit"
    );
    assert!(
        text.contains(&ACP_MAX_INPUT_LINE_BYTES.to_string()),
        "acp.md names the exact input line byte limit"
    );
    assert!(
        text.contains(&format!("{ACP_OUTBOX_CAPACITY} messages")),
        "acp.md names the {ACP_OUTBOX_CAPACITY} message output queue"
    );
    assert!(text.contains("-32700"), "acp.md names the parse-error code");
}
