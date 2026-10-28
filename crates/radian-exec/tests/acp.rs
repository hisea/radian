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
