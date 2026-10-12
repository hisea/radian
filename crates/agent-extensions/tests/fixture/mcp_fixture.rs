//! A small MCP stdio server for the integration tests, written against the wire format
//! rather than an SDK so the tests check the client's real protocol behavior.
//!
//! Usage: `mcp-fixture [--legacy]`
//! - default: 2026-07-28 lifecycle; answers `server/discover`, rejects `initialize`.
//! - `--legacy`: 2025-11-25 lifecycle; rejects `server/discover`, answers `initialize`.
//!
//! If `FIXTURE_LOG` is set, every received method (and cancelled request id) is appended to
//! that file, one per line.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2025-11-25";

struct Server {
    legacy: bool,
    stdout: Mutex<std::io::Stdout>,
    log: Option<Mutex<std::fs::File>>,
    cancelled: Mutex<HashSet<String>>,
    /// Responses to requests this server sent to the client, by request id.
    waiting: Mutex<HashMap<String, mpsc::Sender<Value>>>,
    counter: AtomicU64,
}

impl Server {
    fn send(&self, message: Value) {
        let mut out = self.stdout.lock().unwrap();
        writeln!(out, "{message}").unwrap();
        out.flush().unwrap();
    }

    fn log(&self, line: &str) {
        if let Some(file) = &self.log {
            let mut file = file.lock().unwrap();
            writeln!(file, "{line}").unwrap();
            file.flush().unwrap();
        }
    }

    fn respond(&self, id: &Value, mut result: Value) {
        if !self.legacy {
            result["resultType"] = json!("complete");
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    fn tools(&self) -> Value {
        let object = json!({"type": "object"});
        json!([
            {"name": "echo", "description": "Returns its arguments.", "inputSchema": object},
            {"name": "fail", "description": "Always fails.", "inputSchema": object},
            {"name": "sleep", "description": "Sleeps `ms` milliseconds, ignoring cancellation.",
             "inputSchema": {"type": "object", "properties": {"ms": {"type": "integer"}}, "required": ["ms"]}},
            {"name": "counter", "description": "Counts calls in this process.", "inputSchema": object},
            {"name": "env", "description": "Reads an environment variable.",
             "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}},
            {"name": "spawn_grandchild", "description": "Starts a long-lived child process.", "inputSchema": object},
            {"name": "ask_roots", "description": "Asks the client for roots.", "inputSchema": object},
            {"name": "rich", "description": "Returns several content kinds.", "inputSchema": object},
            {"name": "read_only", "description": "Marked read-only.", "inputSchema": object,
             "annotations": {"readOnlyHint": true}},
            {"name": "Bad Name!", "description": "A name providers reject.", "inputSchema": object},
            {"name": "pid", "description": "Returns this server's process id.", "inputSchema": object},
            {"name": "freeze", "description": "Blocks the whole server, so it stops responding.", "inputSchema": object}
        ])
    }

    fn call(self: &Arc<Self>, id: Value, params: Value) {
        let name = params["name"].as_str().unwrap_or_default().to_owned();
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let text = |t: String| json!({"content": [{"type": "text", "text": t}], "isError": false});
        let result = match name.as_str() {
            "echo" | "read_only" | "Bad Name!" => text(args.to_string()),
            "fail" => {
                json!({"content": [{"type": "text", "text": "fixture failure"}], "isError": true})
            }
            "sleep" => {
                thread::sleep(Duration::from_millis(args["ms"].as_u64().unwrap_or(0)));
                text("slept".into())
            }
            "pid" => text(std::process::id().to_string()),
            "counter" => text((self.counter.fetch_add(1, Ordering::SeqCst) + 1).to_string()),
            "env" => {
                let var = args["name"].as_str().unwrap_or_default();
                text(std::env::var(var).unwrap_or_else(|_| "<unset>".into()))
            }
            "spawn_grandchild" => {
                // Left running on purpose: tests check the host kills the whole process group.
                #[allow(clippy::zombie_processes)]
                let child = std::process::Command::new("sleep")
                    .arg("600")
                    .spawn()
                    .unwrap();
                text(child.id().to_string())
            }
            "ask_roots" => {
                let (tx, rx) = mpsc::channel();
                self.waiting
                    .lock()
                    .unwrap()
                    .insert("fixture-roots".into(), tx);
                self.send(json!({"jsonrpc": "2.0", "id": "fixture-roots", "method": "roots/list"}));
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(reply) => text(reply.to_string()),
                    Err(_) => text("no reply".into()),
                }
            }
            "rich" => json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                    {"type": "resource_link", "uri": "file:///tmp/a.txt", "name": "a.txt"}
                ],
                "structuredContent": {"answer": 42},
                "isError": false
            }),
            other => {
                self.error(&id, -32602, &format!("unknown tool {other}"));
                return;
            }
        };
        // A cancelled request gets no response.
        if !self.cancelled.lock().unwrap().contains(&id.to_string()) {
            self.respond(&id, result);
        }
    }

    fn handle(self: &Arc<Self>, message: Value) {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to one of our requests.
            let key = message["id"].as_str().unwrap_or_default().to_owned();
            if let Some(tx) = self.waiting.lock().unwrap().remove(&key) {
                let _ = tx.send(message);
            }
            return;
        };
        self.log(method);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(json!({}));
        match (method, id) {
            ("server/discover", Some(id)) if self.legacy => self.error(&id, -32601, "Method not found"),
            ("server/discover", Some(id)) => self.respond(
                &id,
                json!({
                    "supportedVersions": [MODERN],
                    "capabilities": {"tools": {}},
                    "ttlMs": 0,
                    "cacheScope": "private",
                    "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "mcp-fixture", "version": "0.0.0"}}
                }),
            ),
            ("initialize", Some(id)) if self.legacy => self.respond(
                &id,
                json!({
                    "protocolVersion": LEGACY,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mcp-fixture", "version": "0.0.0"}
                }),
            ),
            ("initialize", Some(id)) => self.error(&id, -32601, "Method not found"),
            ("notifications/cancelled", None) => {
                let request = params["requestId"].to_string();
                self.log(&format!("cancelled:{request}"));
                self.cancelled.lock().unwrap().insert(request);
            }
            ("tools/list", Some(id)) => {
                let mut result = json!({"tools": self.tools()});
                if !self.legacy {
                    result["ttlMs"] = json!(0);
                    result["cacheScope"] = json!("private");
                }
                self.respond(&id, result);
            }
            ("tools/call", Some(_)) if params["name"] == "freeze" => {
                // Runs on the reading thread, so nothing else is answered from now on.
                thread::sleep(Duration::from_secs(600));
            }
            ("tools/call", Some(id)) => {
                let server = Arc::clone(self);
                thread::spawn(move || server.call(id, params));
            }
            ("ping", Some(id)) => self.respond(&id, json!({})),
            (_, Some(id)) => self.error(&id, -32601, "Method not found"),
            (_, None) => {}
        }
    }
}

fn main() {
    let legacy = std::env::args().any(|a| a == "--legacy");
    let log = std::env::var("FIXTURE_LOG").ok().map(|path| {
        Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
        )
    });
    let server = Arc::new(Server {
        legacy,
        stdout: Mutex::new(std::io::stdout()),
        log,
        cancelled: Mutex::default(),
        waiting: Mutex::default(),
        counter: AtomicU64::new(0),
    });
    server.log(&format!("pid:{}", std::process::id()));
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Ok(message) = serde_json::from_str::<Value>(&line) {
            server.handle(message);
        }
    }
}
