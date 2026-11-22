#![allow(dead_code)]
//! Shared spawn helpers and mock-server fixtures for the `cmd_*.rs` end-to-end tests: the
//! headless command (`radian exec`) against a mock model server and the real MCP
//! fixture, as subprocesses. It used to drive the `agent` binary (ADR 0032): every
//! `agent <args>` is now `radian exec <args>`, and the management subcommands are top-level
//! (`radian trust`, `radian login`, `radian config`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_provider_anthropic::testing as anth;
use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;

pub const KEY: &str = "sk-cli-test-secret";

/// How long a test waits for the CLI process to exit before calling it stuck. These bounds
/// are hang watchdogs, not performance assertions: the actual synchronization is the state
/// condition (process exit, a marker on stderr, a pid disappearing). The slowest legitimate
/// run observed standalone was ~13 s, and one exceeded 30 s on a machine at load average 51,
/// so 120 s leaves roughly ten times the observed worst case while still failing a genuinely
/// stuck CLI.
pub const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

/// The same idea for waits on a state that is not process exit (a stderr marker, a pid going
/// away, a mock server recording a request).
pub const STATE_WATCHDOG: Duration = Duration::from_secs(30);

// ---------- helpers ----------

/// The MCP fixture built next to `radian`. `cargo test -p radian-exec` alone does not
/// necessarily build it, so the failure says what to run.
pub fn fixture_bin() -> PathBuf {
    let path = crate::common::target_dir().join("mcp-fixture");
    assert!(
        path.exists(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    path
}

pub fn write_file(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// Writes an MCP config pointing at the fixture, logging to `log`.
pub fn mcp_config(dir: &Path, log: &Path) -> PathBuf {
    mcp_config_with(dir, log, &[], None)
}

/// Like [`mcp_config`], with extra environment for the fixture and a shutdown timeout.
pub fn mcp_config_with(
    dir: &Path,
    log: &Path,
    env: &[(&str, &str)],
    shutdown_timeout_secs: Option<u64>,
) -> PathBuf {
    let mut server_env = json!({"FIXTURE_LOG": log});
    for (key, value) in env {
        server_env[*key] = json!(value);
    }
    let mut server = json!({
        "id": "fx",
        "command": fixture_bin(),
        "env": server_env,
        "call_timeout_secs": 30,
    });
    if let Some(secs) = shutdown_timeout_secs {
        server["shutdown_timeout_secs"] = json!(secs);
    }
    write_file(dir, "mcp.json", &json!({"servers": [server]}).to_string())
}

pub fn log_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

pub fn text_reply(text: &str) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
    ])
}

pub fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ])
}

pub const ECHO_CALL: &str = "{\"hello\": \"mcp\"}";

/// The model asks for `fx__echo` once, then the given final text.
pub fn echo_then(final_text: &str) -> Vec<Reply> {
    vec![
        tool_reply("call_1", "fx__echo", ECHO_CALL),
        text_reply(final_text),
    ]
}

/// A fresh session directory per spawn. Tests must never write to the user's real data
/// directory; a test that needs two processes to share sessions passes `--session-dir`
/// explicitly, which overrides this.
pub fn fresh_session_dir() -> PathBuf {
    crate::common::process_dir("radian-exec-sessions")
}

/// Writes `credentials/auth.json` (0600) under `root` for the key env vars in `envs` (ADR
/// 0026 D2: the key comes only from `auth.json`). `ANTHROPIC_API_KEY` maps to the `anthropic`
/// descriptor; `OPENAI_API_KEY` and any other `*KEY` name to the default `openai-compatible`.
pub fn write_auth_json(root: &Path, envs: &[(&str, &str)]) {
    let base_url = envs
        .iter()
        .find(|(name, _)| *name == "RADIAN_BASE_URL")
        .map(|(_, value)| *value);
    let mut entries = serde_json::Map::new();
    for (name, value) in envs {
        let provider = if *name == "ANTHROPIC_API_KEY" {
            "anthropic"
        } else if *name == "OPENCODE_GO_API_KEY" {
            "opencode-go"
        } else if name.ends_with("KEY") {
            "openai-compatible"
        } else {
            continue;
        };
        let mut entry = serde_json::json!({"type": "api_key", "key": value});
        if let Some(url) = base_url {
            entry["env"] = serde_json::json!({ "RADIAN_BASE_URL": url });
        }
        entries.insert(provider.to_string(), entry);
    }
    if entries.is_empty() {
        return;
    }
    let dir = root.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = dir.join("auth.json");
    std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
pub fn spawn_radian(args: &[&str], envs: &[(&str, &str)]) -> Child {
    spawn_radian_command(true, args, envs)
}

/// Spawns `radian <args>` (no `exec`): the top-level management commands (`radian trust`,
/// `radian login`, `radian logout`, `radian config`), which run the same code as
/// `radian exec <command>` (ADR 0032).
pub fn spawn_radian_top(args: &[&str], envs: &[(&str, &str)]) -> Child {
    spawn_radian_command(false, args, envs)
}

/// The one spawn helper: `radian exec <args>` (`exec`) or `radian <args>`.
pub fn spawn_radian_command(exec: bool, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    write_auth_json(&home, envs);
    let mut command = Command::new(crate::common::radian_bin());
    if exec {
        command.arg("exec");
    }
    command
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", fresh_session_dir())
        // Tests must not read the user's catalog cache or reach the real models.dev. A fresh
        // `HOME` isolates the cache; the catalog URL is the fast-failing sink
        // ([`crate::common::fast_catalog_url`]), so a lookup fails fast and the window is unknown.
        // A test that wants the catalog overrides both through `envs`.
        .env("HOME", fresh_session_dir())
        .env("RADIAN_CATALOG_URL", crate::common::fast_catalog_url())
        // No test may read the user's real config file: pin the whole `~/.radian` root and
        // drop `RADIAN_CONFIG` (either could otherwise point at a real file).
        .env("RADIAN_HOME", &home)
        .env_remove("RADIAN_CONFIG")
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn radian")
}

/// Writes all input, closes stdin, and waits with a cap so a stuck CLI fails the test
/// instead of hanging it.
pub async fn feed_and_wait(mut child: Child, input: &str) -> (i32, String, String) {
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .await;
    let output = tokio::time::timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("the CLI did not exit within the watchdog")
        .expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

pub fn sigint(child: &Child) {
    // SAFETY: signal delivery to our own child process.
    unsafe {
        libc::kill(child.id().expect("child pid") as libc::pid_t, libc::SIGINT);
    }
}

pub fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

pub async fn gone_within(pid: u32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

pub fn fixture_pid(log: &Path) -> u32 {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("pid:")?.parse().ok())
        .expect("the fixture logged its pid")
}

/// The `messages` of the n-th recorded request.
pub fn messages_of(server: &MockServer, n: usize) -> Value {
    server.requests()[n].json()["messages"].clone()
}

pub fn last_user_text(messages: &Value) -> String {
    messages
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

pub fn tool_result_content(server: &MockServer, n: usize) -> String {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .unwrap()
}

/// A per-test scratch directory under the system temp directory, removed on drop.
///
/// A **passing** test removes it, so these no longer pile up inside a sandbox's workspace
/// `TMPDIR` (dogfooding D14). A **failing** test keeps it and prints its path, so the state
/// that made it fail is still there to inspect.
pub struct ScratchDir(pub PathBuf);

impl std::ops::Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("[scratch] kept for debugging: {}", self.0.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn scratch(name: &str) -> ScratchDir {
    // A process-wide counter keeps every call unique: two parallel tests must never
    // share a directory, and `remove_dir_all` below would delete a running test's files.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // /tmp, not $TMPDIR: the sandbox only writes inside the workspace.
    let dir = PathBuf::from("/tmp").join(format!("radian-exec-{name}-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    ScratchDir(dir)
}

/// Polls `predicate` until it holds, bounded; a sleep here is only the poll interval, never
/// the synchronization itself.
pub async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    tokio::time::timeout(STATE_WATCHDOG, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// A child whose streams the test drives incrementally: state is read off stderr (and the
/// mock server) before acting, instead of guessing with sleeps. Exactly one reader per
/// pipe; `finish` drains what is left and waits.
pub struct Running {
    pub child: Child,
    pub stdin: tokio::process::ChildStdin,
    pub stderr: tokio::process::ChildStderr,
    pub stderr_seen: String,
}

impl Running {
    pub fn spawn(args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut child = spawn_radian(args, envs);
        let stdin = child.stdin.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Self {
            child,
            stdin,
            stderr,
            stderr_seen: String::new(),
        }
    }

    pub async fn write(&mut self, data: &str) {
        self.stdin.write_all(data.as_bytes()).await.unwrap();
    }

    /// Reads more stderr until `marker` appears; keeps everything read so far.
    pub async fn wait_for_stderr(&mut self, marker: &str) {
        timeout(STATE_WATCHDOG, async {
            while !self.stderr_seen.contains(marker) {
                let mut buf = [0u8; 4096];
                let n = self.stderr.read(&mut buf).await.expect("read stderr");
                if n == 0 {
                    panic!(
                        "stderr closed before {marker:?}; seen: {}",
                        self.stderr_seen
                    );
                }
                self.stderr_seen
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {marker:?}; seen: {}",
                self.stderr_seen
            )
        });
    }

    /// Like `wait_for_stderr`, but waits until the marker has been seen `count` times. Needed
    /// where the same prompt legitimately appears more than once.
    pub async fn wait_for_stderr_count(&mut self, marker: &str, count: usize) {
        timeout(STATE_WATCHDOG, async {
            while self.stderr_seen.matches(marker).count() < count {
                let mut buf = [0u8; 4096];
                let read = self.stderr.read(&mut buf).await.expect("read stderr");
                if read == 0 {
                    panic!(
                        "stderr closed before {count} x {marker:?}; seen: {}",
                        self.stderr_seen
                    );
                }
                self.stderr_seen
                    .push_str(&String::from_utf8_lossy(&buf[..read]));
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {count} x {marker:?}; seen: {}",
                self.stderr_seen
            )
        });
    }

    /// Waits for the process to exit **without closing its stdin**, so the test shows the
    /// CLI did not need more input to finish. Bounded by `limit`. The drain appends
    /// each chunk it reads to a shared buffer, so on timeout the panic carries everything
    /// read so far — including lines the CLI printed *during* the hang — plus `note`
    /// (what the test had already done, for example whether the second SIGINT was sent).
    /// A hung CLI still leaves its last known state instead of only "did not exit".
    pub async fn finish_open(self, limit: Duration, note: &str) -> (i32, String, String) {
        let Self {
            child,
            mut stderr,
            stderr_seen,
            stdin,
        } = self;
        let seen = Arc::new(tokio::sync::Mutex::new(stderr_seen));
        let drain_seen = Arc::clone(&seen);
        let note = note.to_owned();
        let (output, ()) = timeout(limit, async {
            tokio::join!(child.wait_with_output(), async {
                let mut chunk = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            drain_seen
                                .lock()
                                .await
                                .push_str(&String::from_utf8_lossy(&chunk[..n]));
                        }
                    }
                }
            })
        })
        .await
        .unwrap_or_else(|_| {
            // The timed future above is dropped before this runs, so the drain is gone
            // and the lock is free; `try_lock` only guards against a surprise.
            let seen_so_far = seen
                .try_lock()
                .map(|guard| guard.clone())
                .unwrap_or_else(|_| "<stderr buffer locked>".to_owned());
            panic!(
                "the CLI did not exit while stdin stayed open within {limit:?} ({note}); stderr so far:\n{seen_so_far}"
            )
        });
        let output = output.expect("wait");
        drop(stdin);
        let stderr_seen = seen.lock().await.clone();
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr_seen,
        )
    }

    /// Closes stdin, drains both pipes, and waits with a bound.
    pub async fn finish(self) -> (i32, String, String) {
        drop(self.stdin);
        let Self {
            child,
            mut stderr,
            mut stderr_seen,
            stdin: _,
        } = self;
        let (output, ()) = timeout(EXIT_WATCHDOG, async {
            tokio::join!(child.wait_with_output(), async {
                let mut rest = String::new();
                let _ = stderr.read_to_string(&mut rest).await;
                stderr_seen.push_str(&rest);
            })
        })
        .await
        .expect("the CLI did not exit in time");
        let output = output.expect("wait");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr_seen,
        )
    }
}

/// Like [`spawn_radian`], but uses a caller-owned session directory, so a test can assert
/// that a failed start left it empty (dogfooding D2).
pub fn spawn_radian_in(session_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    write_auth_json(&home, envs);
    Command::new(crate::common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", session_dir)
        // Isolate the catalog cache, the `~/.radian` root and the network, like `spawn_radian`.
        .env("HOME", fresh_session_dir())
        .env("RADIAN_CATALOG_URL", crate::common::fast_catalog_url())
        .env("RADIAN_HOME", &home)
        .env_remove("RADIAN_CONFIG")
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn agent")
}

pub fn assert_session_dir_empty(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.is_empty(),
        "a failed start left files behind: {names:?}"
    );
}

pub fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The tool names the adapter advertised in request `n`.
pub fn advertised_tools(server: &MockServer, n: usize) -> Vec<String> {
    server.requests()[n].json()["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// A stdout whose reader is already gone, so the child's first write is `EPIPE` no matter how
/// much it writes: the read end is closed before the child starts, which does not depend on
/// the pipe buffer size. `SIGPIPE` stays ignored (Rust's default), so the write returns
/// `BrokenPipe` instead of killing the process.
#[cfg(unix)]
pub fn closed_stdout() -> Stdio {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: a fresh pipe; both fds are owned here.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: closing a fd we own.
    unsafe { libc::close(fds[0]) };
    // SAFETY: `fds[1]` is an owned write fd, moved into the child's stdout.
    unsafe { Stdio::from_raw_fd(fds[1]) }
}

/// The `.jsonl` session files in a directory, name and bytes. `index.db` and its WAL files
/// are a disposable cache (ADR 0006 §10), not session files, so they are excluded.
pub fn jsonl_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut snapshot: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&path).unwrap(),
            )
        })
        .collect();
    snapshot.sort();
    snapshot
}

/// The sorted file names in a directory, for the "writes nothing" assertion.
pub fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

pub fn budget_workspace(label: &str) -> (ScratchDir, PathBuf) {
    let dir = scratch(label);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (dir, ws)
}

/// A workspace and session directory for one subtree test. `$TMPDIR`, not `/tmp`: the tests
/// do not open a control socket, and this keeps the tree writable inside the workspace
/// sandbox. The workspace is its own repository root, so discovery stops there.
pub fn subtree_scratch(label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = crate::common::process_dir(&format!("radian-exec-{label}"));
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let sessions = fresh_session_dir();
    (dir, ws, sessions)
}

/// Like [`spawn_radian`], but without the default `RADIAN_SESSION_DIR`, so a test controls the
/// session location (`RADIAN_HOME` / `RADIAN_SESSION_DIR` / `--session-dir`).
pub fn spawn_radian_bare(args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    let root = envs
        .iter()
        .find(|(name, _)| *name == "RADIAN_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .unwrap_or_else(|| home.join(".radian"));
    write_auth_json(&root, envs);
    Command::new(crate::common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env_remove("RADIAN_SESSION_DIR")
        // The whole root is the test's to control, but it still must not be the real one:
        // a fresh `HOME` makes the `$HOME/.radian` fallback hermetic, and `RADIAN_CONFIG`
        // (which would win over both) is dropped.
        .env("HOME", fresh_session_dir())
        .env_remove("RADIAN_HOME")
        .env_remove("RADIAN_CONFIG")
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn agent")
}

/// One Anthropic text reply: the minimal end_turn event stream (wire notes §4).
pub fn anth_text_reply(text: &str) -> anth::Reply {
    anth::Reply::sse(&[
        anth::message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        anth::text_start(0),
        anth::text_delta(0, text),
        anth::block_stop(0),
        anth::message_delta("end_turn", Some(json!({"output_tokens": 2}))),
        anth::message_stop(),
    ])
}

/// Writes a minimal, valid v1 session file by hand: one user message and one assistant
/// message, optionally carrying reasoning and a raw provider payload. Used where a real run
/// cannot produce the field (the OpenAI adapter never sets a provider payload).
pub fn write_session(
    dir: &Path,
    id: &str,
    workspace: &str,
    assistant_text: &str,
    reasoning: Option<&str>,
    payload: Option<&str>,
) -> PathBuf {
    let ts = "2026-10-13T00:00:00.000000Z";
    let mut assistant = json!({"content": [{"text": {"text": assistant_text}}]});
    if let Some(reasoning) = reasoning {
        assistant["reasoning"] = json!([{"text": {"text": reasoning}}]);
    }
    if let Some(payload) = payload {
        assistant["provider_payload"] = serde_json::from_str(payload).unwrap();
    }
    let lines = [
        json!({"schema_version":1,"session_id":id,"seq":0,"ts":ts,"event":"SessionCreated",
               "critical":true,"data":{"workspace":workspace,"config_snapshot":{}}}),
        json!({"schema_version":1,"session_id":id,"seq":1,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":0,
               "message":{"user":{"content":[{"text":{"text":"hello from the user"}}]}}}}),
        json!({"schema_version":1,"session_id":id,"seq":2,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":1,"message":{"assistant":assistant}}}),
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
