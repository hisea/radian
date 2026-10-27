//! A minimal Agent Client Protocol v1 client over stdio (ADR 0048 decision 7).
//!
//! The crate the protocol publishes, `agent-client-protocol`, is an asynchronous client built
//! on `async-io`; this wrapper is a synchronous process whose whole design is one poll loop
//! around a child, its output stream and its deadlines. So this module speaks the protocol's
//! JSON-RPC itself: one request at a time, a reader thread that parses the agent's stdout and
//! writes every protocol line where the caller asks, and a writer that serialises requests on
//! the child's stdin. It assumes nothing about the agent beyond ACP v1, so a later arc worker
//! can reuse it for any agent, `radian acp` included.
//!
//! The client implements the one client method an agent may call, `session/request_permission`
//! (v0 advertises no `fs/*` or `terminal/*` capability), and the two notifications it needs:
//! `session/update` is copied out, and `session/cancel` says a turn should end.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// The protocol version this client speaks. ACP v2 is a draft; the wrapper stays on v1.
pub const PROTOCOL_VERSION: u64 = 1;

/// The attempt's `result.jsonl`, shared by the reader thread and the wrapper. Cloning the
/// [`Arc`] is what lets the wrapper write the final `Result` after the reader has stopped.
pub type Log = Arc<Mutex<File>>;

/// Appends one line to the attempt's `result.jsonl`. A failed write is not the attempt's
/// failure: the wrapper's own records carry the outcome, and the log is for people.
pub fn append(log: &Log, value: &Value) {
    if let Ok(mut file) = log.lock() {
        let _ = write_json_line(&mut *file, value);
    }
}

/// One turn's end: the agent's `stopReason`, the last contiguous run of message text, and the
/// response's `_meta` object (ADR 0049: `_meta.radian` carries what ACP has no field for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEnd {
    pub stop_reason: String,
    /// The text of the last contiguous run of `agent_message_chunk` updates, as sent.
    pub text: String,
    /// The `_meta` object of the `session/prompt` response, or `Value::Null` when it has none.
    pub meta: Value,
}

/// The client of one agent process.
pub struct Client {
    shared: Arc<Shared>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_id: AtomicU64,
}

struct Shared {
    log: Log,
    /// Our requests' responses, by request id.
    responses: Mutex<BTreeMap<u64, Result<Value, String>>>,
    /// Set by [`Client::cancel`]; every later `session/request_permission` is answered
    /// `cancelled` instead of by the path policy.
    cancelled: AtomicBool,
    /// How many protocol lines the reader has seen. The wrapper's stall timer reads it: an
    /// agent that says nothing at all for the stall window is stopped, as a silent command
    /// worker is.
    activity: AtomicU64,
    /// The last contiguous run of `agent_message_chunk` text. Any other `session/update`
    /// clears it.
    message_run: Mutex<String>,
    /// The attempt's worktree: the one directory a permission request may name.
    worktree: PathBuf,
    /// The id `session/new` returned.
    session: Mutex<Option<String>>,
}

impl Client {
    /// Starts the reader on an already spawned agent. The child must have piped stdin and
    /// stdout; `stderr` is the caller's. `worktree` is the attempt's worktree, which decides
    /// permission requests, and `log` is the attempt's `result.jsonl`.
    pub fn start(child: &mut Child, worktree: PathBuf, log: Log) -> Result<Self, String> {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "the agent has no stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "the agent has no stdout".to_owned())?;
        let shared = Arc::new(Shared {
            log,
            responses: Mutex::new(BTreeMap::new()),
            cancelled: AtomicBool::new(false),
            activity: AtomicU64::new(0),
            message_run: Mutex::new(String::new()),
            worktree,
            session: Mutex::new(None),
        });
        let writer = Arc::new(Mutex::new(Some(stdin)));
        let reader_shared = Arc::clone(&shared);
        let reader_stdin = Arc::clone(&writer);
        let reader = std::thread::spawn(move || read_agent(reader_shared, reader_stdin, stdout));
        Ok(Self {
            shared,
            stdin: writer,
            reader: Some(reader),
            next_id: AtomicU64::new(1),
        })
    }

    /// Sends one request and returns its id. The caller waits for the response with
    /// [`Client::has_response`] while its own loop enforces the attempt's limits.
    pub fn request(&self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        Ok(id)
    }

    /// Whether `id`'s response has arrived.
    pub fn has_response(&self, id: u64) -> bool {
        self.shared
            .responses
            .lock()
            .is_ok_and(|responses| responses.contains_key(&id))
    }

    /// Takes `id`'s response. `None` before it arrives; a response the agent sent as an error
    /// is the `Err` text.
    pub fn take_response(&self, id: u64) -> Option<Result<Value, String>> {
        self.shared
            .responses
            .lock()
            .ok()
            .and_then(|mut responses| responses.remove(&id))
    }

    /// The stall timer's input: how many protocol lines have arrived.
    pub fn activity(&self) -> u64 {
        self.shared.activity.load(Ordering::SeqCst)
    }

    /// The last contiguous run of message text, for `Result.text`.
    pub fn message_text(&self) -> String {
        self.shared
            .message_run
            .lock()
            .map(|run| run.clone())
            .unwrap_or_default()
    }

    /// Records the session `session/new` returned; `session/cancel` needs it.
    pub fn set_session(&self, id: &str) {
        if let Ok(mut session) = self.shared.session.lock() {
            *session = Some(id.to_owned());
        }
    }

    /// Asks the agent to end the turn. From now on every permission request is answered
    /// `cancelled`; the caller then stops the process group.
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::SeqCst);
        let session = self
            .shared
            .session
            .lock()
            .ok()
            .and_then(|session| session.clone());
        if let Some(session) = session {
            let _ = self.send(&json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session },
            }));
        }
    }

    /// Closes the agent's stdin: the protocol stream's end. A well-behaved agent treats it as
    /// the end of the session and exits; the caller waits a moment before it signals the group.
    pub fn close_input(&self) {
        if let Ok(mut stdin) = self.stdin.lock() {
            *stdin = None;
        }
    }

    /// Closes the agent's stdin and waits for the reader to reach the end of its output. The
    /// caller has stopped and reaped the process by then.
    pub fn finish(&mut self) {
        self.close_input();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }

    fn send(&self, value: &Value) -> Result<(), String> {
        send_on(&self.stdin, value)
    }
}

/// One message on the agent's stdin. The lock covers the whole message, so two senders never
/// interleave halves of a line.
fn send_on(stdin: &Mutex<Option<ChildStdin>>, value: &Value) -> Result<(), String> {
    let mut guard = stdin
        .lock()
        .map_err(|_| "the agent's stdin lock is poisoned".to_owned())?;
    let Some(stdin) = guard.as_mut() else {
        return Err("the agent's stdin is closed".to_owned());
    };
    write_json_line(stdin, value).map_err(|error| format!("cannot write to the agent: {error}"))
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

/// The reader: every line is a JSON-RPC message. A line that is not JSON, or not a JSON
/// object with a method or an id, is not part of the protocol and is skipped.
fn read_agent(shared: Arc<Shared>, stdin: Arc<Mutex<Option<ChildStdin>>>, stdout: ChildStdout) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else {
            return;
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        shared.activity.fetch_add(1, Ordering::SeqCst);
        if value.get("method").and_then(Value::as_str).is_some() {
            handle_agent_message(&shared, &stdin, &value);
        } else if let Some(id) = value.get("id").and_then(Value::as_u64) {
            let outcome = match value.get("error") {
                Some(error) => Err(error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("the agent returned an error")
                    .to_owned()),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            if let Ok(mut responses) = shared.responses.lock() {
                responses.insert(id, outcome);
            }
        }
    }
}

/// A request or notification from the agent. `session/update` and `session/request_permission`
/// are the two a v1 agent sends a client that advertises no capabilities.
fn handle_agent_message(shared: &Shared, stdin: &Mutex<Option<ChildStdin>>, value: &Value) {
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match method {
        "session/update" => {
            append(&shared.log, &json!({"type": "acp", "message": value}));
            let Some(update) = value.pointer("/params/update") else {
                return;
            };
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut run = shared
                .message_run
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if kind == "agent_message_chunk" {
                if let Some(text) = update.pointer("/content/text").and_then(Value::as_str) {
                    run.push_str(text);
                }
            } else {
                run.clear();
            }
        }
        "session/request_permission" => {
            append(&shared.log, &json!({"type": "acp", "message": value}));
            let params = value.get("params").cloned().unwrap_or(Value::Null);
            let choice = if shared.cancelled.load(Ordering::SeqCst) {
                Choice::Cancelled
            } else {
                permission_choice(&params, &shared.worktree)
            };
            let answer = answer_for(&params, choice);
            append(
                &shared.log,
                &json!({
                    "type": "acp",
                    "answer": {
                        "requestId": value.get("id").cloned().unwrap_or(Value::Null),
                        "kind": answer.kind,
                        "optionId": answer.option_id,
                    },
                }),
            );
            let Some(id) = value.get("id") else {
                return;
            };
            let result = match &answer.option_id {
                Some(option) => json!({
                    "outcome": { "outcome": "selected", "optionId": option },
                }),
                None => json!({ "outcome": { "outcome": "cancelled" } }),
            };
            let _ = send_on(
                stdin,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            );
        }
        _ => {
            // A method this client does not implement: answer the request with the protocol's
            // method-not-found error. A notification has no id and needs no answer.
            if let Some(id) = value.get("id") {
                let _ = send_on(
                    stdin,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("method `{method}` is not implemented"),
                        },
                    }),
                );
            }
        }
    }
}

/// The policy decision for one permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Allow,
    Reject,
    Cancelled,
}

struct Answer {
    kind: &'static str,
    option_id: Option<String>,
}

/// The option the agent offered for `choice`'s kind. A request that offers no matching option
/// is answered `cancelled`: this client never invents an option.
fn answer_for(params: &Value, choice: Choice) -> Answer {
    let wanted = match choice {
        Choice::Allow => "allow_once",
        Choice::Reject => "reject_once",
        Choice::Cancelled => {
            return Answer {
                kind: "cancelled",
                option_id: None,
            };
        }
    };
    let option_id = params
        .get("options")
        .and_then(Value::as_array)
        .and_then(|options| {
            options.iter().find_map(|option| {
                (option.get("kind").and_then(Value::as_str) == Some(wanted))
                    .then(|| option.get("optionId").and_then(Value::as_str))
                    .flatten()
                    .map(str::to_owned)
            })
        });
    match option_id {
        Some(option_id) => Answer {
            kind: wanted,
            option_id: Some(option_id),
        },
        None => Answer {
            kind: "cancelled",
            option_id: None,
        },
    }
}

/// The v0 permission policy: a request whose paths all lie inside the attempt's worktree is
/// allowed once; one path outside it rejects the request. A request that names no path counts
/// as inside, because there is nothing to place outside (ADR 0048 decision 7).
fn permission_choice(params: &Value, worktree: &Path) -> Choice {
    let mut paths = Vec::new();
    if let Some(tool_call) = params.get("toolCall") {
        collect_paths(tool_call, &mut paths);
    }
    if paths.iter().all(|path| inside_worktree(worktree, path)) {
        Choice::Allow
    } else {
        Choice::Reject
    }
}

/// Every string under a `path` key in the tool call: ACP's `locations` carries them, and some
/// agents also put one in `rawInput`. A `path` that is not a string is ignored.
fn collect_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "path" {
                    if let Some(path) = value.as_str() {
                        out.push(path.to_owned());
                    }
                } else {
                    collect_paths(value, out);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_paths(item, out);
            }
        }
        _ => {}
    }
}

/// Whether `path` (absolute, or relative to the worktree) lies inside `worktree`. Both sides
/// are canonicalized when they exist, so `..` and a symbolic link cannot name a path outside
/// while reading as one inside; a path that does not exist yet is normalized lexically.
fn inside_worktree(worktree: &Path, path: &str) -> bool {
    let root = canonical_or_normalized(worktree);
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    canonical_or_normalized(&joined).starts_with(&root)
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(paths: &[&str]) -> Value {
        json!({
            "sessionId": "s",
            "toolCall": {
                "toolCallId": "c",
                "locations": paths.iter().map(|path| json!({"path": path})).collect::<Vec<_>>(),
            },
            "options": [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            ],
        })
    }

    /// A request inside the worktree is allowed once, one outside rejects it, and no path at
    /// all counts as inside (ADR 0048 decision 7).
    #[test]
    fn the_permission_policy_is_inside_the_worktree() {
        let root = Path::new("/tmp/arc/worktrees/TK0001-a");
        assert_eq!(
            permission_choice(&params(&["/tmp/arc/worktrees/TK0001-a/image.txt"]), root),
            Choice::Allow
        );
        assert_eq!(
            permission_choice(&params(&["/tmp/arc/worktrees/TK0001-a/../other/x"]), root),
            Choice::Reject
        );
        assert_eq!(
            permission_choice(&params(&["/tmp/arc/worktrees/TK0001-a-other/x"]), root),
            Choice::Reject,
            "a shared prefix is not a subdirectory"
        );
        assert_eq!(permission_choice(&params(&[]), root), Choice::Allow);
        let no_tool_call = json!({"options": []});
        assert_eq!(permission_choice(&no_tool_call, root), Choice::Allow);
    }

    /// The answer names the option the agent offered; a request with no matching option is
    /// cancelled rather than guessed at.
    #[test]
    fn the_answer_is_the_agents_own_option() {
        let request = params(&["/tmp/arc/worktrees/TK0001-a/image.txt"]);
        let allow = answer_for(&request, Choice::Allow);
        assert_eq!(allow.kind, "allow_once");
        assert_eq!(allow.option_id.as_deref(), Some("allow"));
        let reject = answer_for(&request, Choice::Reject);
        assert_eq!(reject.kind, "reject_once");
        assert_eq!(reject.option_id.as_deref(), Some("reject"));
        let cancelled = answer_for(&json!({"options": []}), Choice::Allow);
        assert_eq!(cancelled.kind, "cancelled");
        assert_eq!(cancelled.option_id, None);
    }
}
