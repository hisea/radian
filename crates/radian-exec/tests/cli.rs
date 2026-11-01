//! The headless command (`radian exec`) against a mock model server and the real MCP
//! fixture, as subprocesses. It used to drive the `agent` binary (ADR 0032): every
//! `agent <args>` is now `radian exec <args>`, and the management subcommands are top-level
//! (`radian memory`, `radian login`, `radian config`).

mod common;

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

const KEY: &str = "sk-cli-test-secret";

/// How long a test waits for the CLI process to exit before calling it stuck. These bounds
/// are hang watchdogs, not performance assertions: the actual synchronization is the state
/// condition (process exit, a marker on stderr, a pid disappearing). The slowest legitimate
/// run observed standalone was ~13 s, and one exceeded 30 s on a machine at load average 51,
/// so 120 s leaves roughly ten times the observed worst case while still failing a genuinely
/// stuck CLI.
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

/// The same idea for waits on a state that is not process exit (a stderr marker, a pid going
/// away, a mock server recording a request).
const STATE_WATCHDOG: Duration = Duration::from_secs(30);

// ---------- helpers ----------

/// The MCP fixture built next to `radian`. `cargo test -p radian-exec` alone does not
/// necessarily build it, so the failure says what to run.
fn fixture_bin() -> PathBuf {
    let path = common::target_dir().join("mcp-fixture");
    assert!(
        path.exists(),
        "mcp-fixture is not built; run: cargo build -p agent-extensions --bin mcp-fixture"
    );
    path
}

fn write_file(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// Writes an MCP config pointing at the fixture, logging to `log`.
fn mcp_config(dir: &Path, log: &Path) -> PathBuf {
    mcp_config_with(dir, log, &[], None)
}

/// Like [`mcp_config`], with extra environment for the fixture and a shutdown timeout.
fn mcp_config_with(
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

fn log_lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

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

const ECHO_CALL: &str = "{\"hello\": \"mcp\"}";

/// The model asks for `fx__echo` once, then the given final text.
fn echo_then(final_text: &str) -> Vec<Reply> {
    vec![
        tool_reply("call_1", "fx__echo", ECHO_CALL),
        text_reply(final_text),
    ]
}

/// A fresh session directory per spawn. Tests must never write to the user's real data
/// directory; a test that needs two processes to share sessions passes `--session-dir`
/// explicitly, which overrides this.
fn fresh_session_dir() -> PathBuf {
    common::process_dir("radian-exec-sessions")
}

/// Writes `credentials/auth.json` (0600) under `root` for the key env vars in `envs` (ADR
/// 0026 D2: the key comes only from `auth.json`). `ANTHROPIC_API_KEY` maps to the `anthropic`
/// descriptor; `OPENAI_API_KEY` and any other `*KEY` name to the default `openai-compatible`.
fn write_auth_json(root: &Path, envs: &[(&str, &str)]) {
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
fn spawn_radian(args: &[&str], envs: &[(&str, &str)]) -> Child {
    spawn_radian_command(true, args, envs)
}

/// Spawns `radian <args>` (no `exec`): the top-level management commands (`radian memory`,
/// `radian login`, `radian logout`, `radian config`), which run the same code as
/// `radian exec <command>` (ADR 0032).
fn spawn_radian_top(args: &[&str], envs: &[(&str, &str)]) -> Child {
    spawn_radian_command(false, args, envs)
}

/// The one spawn helper: `radian exec <args>` (`exec`) or `radian <args>`.
fn spawn_radian_command(exec: bool, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    write_auth_json(&home, envs);
    let mut command = Command::new(common::radian_bin());
    if exec {
        command.arg("exec");
    }
    command
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", fresh_session_dir())
        // Tests must not read the user's catalog cache or reach the real models.dev. A fresh
        // `HOME` isolates the cache; the catalog URL is the fast-failing sink
        // ([`common::fast_catalog_url`]), so a lookup fails fast and the window is unknown.
        // A test that wants the catalog overrides both through `envs`.
        .env("HOME", fresh_session_dir())
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
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
async fn feed_and_wait(mut child: Child, input: &str) -> (i32, String, String) {
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

/// The number of `TurnFinished` events across the session files in `sessions`.
fn journal_turns(sessions: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .map(|entry| {
            std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .matches("\"event\":\"TurnFinished\"")
                .count()
        })
        .sum()
}

fn sigint(child: &Child) {
    // SAFETY: signal delivery to our own child process.
    unsafe {
        libc::kill(child.id().expect("child pid") as libc::pid_t, libc::SIGINT);
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks for existence.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn gone_within(pid: u32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn fixture_pid(log: &Path) -> u32 {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("pid:")?.parse().ok())
        .expect("the fixture logged its pid")
}

/// The `messages` of the n-th recorded request.
fn messages_of(server: &MockServer, n: usize) -> Value {
    server.requests()[n].json()["messages"].clone()
}

fn last_user_text(messages: &Value) -> String {
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

fn tool_result_content(server: &MockServer, n: usize) -> String {
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
struct ScratchDir(PathBuf);

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

fn scratch(name: &str) -> ScratchDir {
    // A process-wide counter keeps every call unique: two parallel tests must never
    // share a directory, and `remove_dir_all` below would delete a running test's files.
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // /tmp, not $TMPDIR: on macOS the latter can push <home>/run/<pid>.sock past SUN_LEN (104).
    let dir = PathBuf::from("/tmp").join(format!("radian-exec-{name}-{n}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    ScratchDir(dir)
}

/// Polls `predicate` until it holds, bounded; a sleep here is only the poll interval, never
/// the synchronization itself.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
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
struct Running {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stderr: tokio::process::ChildStderr,
    stderr_seen: String,
}

impl Running {
    fn spawn(args: &[&str], envs: &[(&str, &str)]) -> Self {
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

    async fn write(&mut self, data: &str) {
        self.stdin.write_all(data.as_bytes()).await.unwrap();
    }

    /// Reads more stderr until `marker` appears; keeps everything read so far.
    async fn wait_for_stderr(&mut self, marker: &str) {
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
    async fn wait_for_stderr_count(&mut self, marker: &str, count: usize) {
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
    async fn finish_open(self, limit: Duration, note: &str) -> (i32, String, String) {
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
    async fn finish(self) -> (i32, String, String) {
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

// ---------- streaming and exit codes ----------

#[tokio::test]
async fn one_shot_streams_text_to_stdout_only() {
    let server = MockServer::start(vec![text_reply("Hello there")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "Hello there\n");
    assert!(!stderr.contains("Hello there"), "{stderr}");
    // The first message is the system prompt (the skill list is in it); the user's words are
    // the user message.
    assert_eq!(last_user_text(&messages_of(&server, 0)), "hi");
}

#[tokio::test]
async fn a_model_error_exits_one() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"bad key\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("HTTP 401"), "{stderr}");
}

/// ADR 0032: the diagnostics say `radian: …`, not `agent: …` (the binary is
/// gone; `radian exec` is the headless command). Every failure line carries the prefix.
#[tokio::test]
async fn diagnostics_carry_the_radian_prefix() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"bad key\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.lines().any(|line| line.starts_with("radian: ")),
        "a diagnostic line starts with `radian: `: {stderr}"
    );
    assert!(
        !stderr.contains("agent: "),
        "no `agent: ` line remains: {stderr}"
    );
}

// ---------- usage and configuration errors ----------

#[tokio::test]
async fn usage_errors_exit_two() {
    let (code, _, _) = feed_and_wait(spawn_radian(&["-p", "hi"], &[]), "").await; // no --model
    assert_eq!(code, 2);
}

/// ADR 0018 removed `--context-threshold`; passing it is a usage error (exit 2), never a
/// silently ignored flag.
#[tokio::test]
async fn the_removed_context_threshold_flag_is_an_error() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--context-threshold",
                "80",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--context-threshold"), "{stderr}");
}

#[tokio::test]
async fn bad_mcp_config_exits_two_with_the_path() {
    let dir = scratch("config");

    // Unknown field.
    let unknown = write_file(&dir, "unknown.json", "{\"servers\": [], \"bogus\": 1}");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                unknown.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(unknown.to_str().unwrap()), "{stderr}");

    // Missing file.
    let missing = dir.join("no-such-file.json");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                missing.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");

    // Duplicate ids, caught before anything starts.
    let dup = write_file(
        &dir,
        "dup.json",
        &json!({"servers": [
            {"id": "fx", "command": fixture_bin()},
            {"id": "fx", "command": fixture_bin()}
        ]})
        .to_string(),
    );
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "-p",
                "hi",
                "--mcp-config",
                dup.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains(dup.to_str().unwrap()), "{stderr}");
    assert!(stderr.contains("duplicate"), "{stderr}");
}

// ---------- the API key ----------

#[tokio::test]
async fn the_key_comes_from_the_named_env_and_never_leaks() {
    // The error body echoes the key back; neither stream may carry it.
    let server = MockServer::start(vec![Reply::status(
        401,
        &format!("{{\"error\":{{\"message\":\"bad {KEY}\"}}}}"),
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXEC_TEST_KEY", KEY)],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
    assert!(!stdout.contains(KEY), "stdout leaked the key");
    assert!(!stderr.contains(KEY), "stderr leaked the key");
}

/// ADR 0013 T4: a runtime error is redacted with **every** login in
/// `auth.json`, not only the route's key. The server echoes a key that belongs to another
/// provider; the adapter cannot mask that one (it only knows its own key), so the host must.
#[tokio::test]
async fn a_runtime_error_redacts_every_auth_key_not_only_the_route_key() {
    let route_key = "sk-ROUTE-KEY-111111";
    let other_key = "sk-OTHER-PROVIDER-KEY-222222";
    let server = MockServer::start(vec![Reply::status(
        401,
        &format!("{{\"error\":{{\"message\":\"bad {other_key}\"}}}}"),
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[
                ("OPENAI_API_KEY", route_key),
                ("ANTHROPIC_API_KEY", other_key),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some(format!("Bearer {route_key}").as_str())
    );
    assert!(
        !stderr.contains(other_key),
        "the runtime error leaked another provider's key: {stderr}"
    );
    assert!(!stdout.contains(other_key), "stdout leaked it: {stdout}");
    assert!(stderr.contains("<redacted>"), "{stderr}");
}

/// ADR 0031 T3: a non-interactive run with project resources is untrusted by default, prints
/// the one-line way out, and writes nothing.
#[tokio::test]
async fn a_workspace_with_project_resources_is_untrusted_without_a_terminal() {
    let dir = scratch("trust-default");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    let server = MockServer::start(vec![text_reply("TRUST-OK")]).await;
    let (code, _stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "--session-id",
                "trust-default",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0);
    assert!(
        stderr.contains("project resources not loaded (untrusted)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("project resources: not loaded (untrusted: default)"),
        "{stderr}"
    );
    assert!(stderr.contains("--trust"), "{stderr}");
    assert!(
        !home.join("trust/trust.json").exists(),
        "nothing is written"
    );
}

/// ADR 0031 T3: `--trust`/`--no-trust` override for this process only and never write.
#[tokio::test]
async fn trust_flags_override_without_writing() {
    let dir = scratch("trust-flags");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    // One reply per iteration: `-p` runs a turn each time.
    let server = MockServer::start(vec![text_reply("OK"), text_reply("OK")]).await;
    for (flag, expected) in [
        (
            "--trust",
            "project resources: loaded (trusted: command line)",
        ),
        (
            "--no-trust",
            "project resources: not loaded (untrusted: command line)",
        ),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "-p",
                    "hi",
                    flag,
                    "--workspace",
                    ws.to_str().unwrap(),
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--session-id",
                    if flag == "--trust" {
                        "trust-flag-a"
                    } else {
                        "trust-flag-b"
                    },
                ],
                &[("RADIAN_HOME", home.to_str().unwrap())],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{flag}: {stderr}");
        assert!(stderr.contains(expected), "{flag}: {stderr}");
        assert!(
            !home.join("trust/trust.json").exists(),
            "{flag} must not write trust.json"
        );
    }
}

/// Project skills (backlog project-skills): a trusted project's `.agents/skills` reach the
/// system prompt, `skill` tool and `/skill:` catalog; an untrusted project's do not. The
/// startup line tells the truth in both states.
#[tokio::test]
async fn project_skills_load_only_when_trusted() {
    let dir = scratch("project-skills-e2e");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\ndescription: PROJECT-SKILL-MARKER greets.\n---\nbody\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    // One reply per iteration: `-p` runs a turn each time.
    let server = MockServer::start(vec![text_reply("OK"), text_reply("OK")]).await;
    for (n, (flag, loaded, status)) in [
        (
            "--trust",
            true,
            "project resources: loaded (trusted: command line)",
        ),
        (
            "--no-trust",
            false,
            "project resources: not loaded (untrusted: command line)",
        ),
    ]
    .iter()
    .enumerate()
    {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "-p",
                    "hi",
                    flag,
                    "--workspace",
                    ws.to_str().unwrap(),
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--session-id",
                    &format!("project-skills-{n}"),
                ],
                &[("RADIAN_HOME", home.to_str().unwrap())],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{flag}: {stderr}");
        assert!(stderr.contains(status), "{flag}: {stderr}");
        let system = messages_of(&server, n)
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "system")
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            system.contains("PROJECT-SKILL-MARKER"),
            *loaded,
            "{flag}: the project skill must{} reach the system prompt: {system}",
            if *loaded { "" } else { " not" }
        );
    }
}

/// ADR 0031 T3: piped stdin is **not** an interactive user. The trust question must not read
/// its first line (which could be a message, or a stray `y`), so a piped run takes the
/// untrusted default and sends the piped line as a message.
#[tokio::test]
async fn piped_stdin_is_never_read_as_the_trust_answer() {
    let dir = scratch("trust-piped");
    let home = dir.0.join("home");
    let ws = dir.0.join("ws");
    std::fs::create_dir_all(ws.join(".agents/skills/greet")).unwrap();
    std::fs::write(
        ws.join(".agents/skills/greet/SKILL.md"),
        "---\nname: greet\n---\n",
    )
    .unwrap();
    let sessions = dir.0.join("sessions");
    let server = MockServer::start(vec![text_reply("PIPED-OK")]).await;
    let (code, _stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "--session-id",
                "trust-piped",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        // A stray `y` would be a wrong "allow" if the question read it.
        "hello from the pipe\n",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    // No question: piped stdin is not a terminal.
    assert!(!stderr.contains("project resources found in"), "{stderr}");
    assert!(
        stderr.contains("project resources: not loaded (untrusted: default)"),
        "{stderr}"
    );
    assert!(
        !home.join("trust/trust.json").exists(),
        "nothing is written"
    );
    // The piped line was sent as an ordinary message, not consumed by a trust answer.
    let messages = server.requests()[0].json()["messages"].clone();
    let user = messages
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(user, "hello from the pipe");
}

#[tokio::test]
async fn a_remote_base_url_without_a_key_fails_fast() {
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "https://api.example.com/v1",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    // ADR 0026 D2/D3: the message names the fix — `radian login <provider>` (which writes
    // `credentials/auth.json`) — and names the redacted endpoint.
    assert!(
        stderr.contains("radian login openai-compatible"),
        "{stderr}"
    );
    assert!(stderr.contains("/login"), "{stderr}");
    assert!(stderr.contains("https://api.example.com/v1"), "{stderr}");
    // The message must not suggest a placeholder key value or embed a JSON template with a
    // key slot any more: `/login` asks for the key without echoing it.
    assert!(!stderr.contains("MY_KEY"), "{stderr}");
    // No network round trip was attempted.
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn a_local_base_url_needs_no_key_and_sends_no_authorization() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, stdout, _) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "ok\n");
    assert_eq!(server.requests()[0].header("authorization"), None);
}

/// ADR 0026 D2: `--api-key-env` is removed, so passing it is a usage error (the key comes only
/// from `auth.json`). This is the mutation guard for re-adding the flag.
#[tokio::test]
async fn the_removed_api_key_env_flag_is_a_usage_error() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--api-key-env", "SOME_KEY", "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--api-key-env"), "{stderr}");
    assert!(stderr.contains("unexpected argument"), "{stderr}");
}

/// ADR 0026 D0: `--base-url`/`--header` only apply to the generic descriptor; a non-generic
/// descriptor sets its own endpoint and headers, so passing them is a usage error.
#[tokio::test]
async fn a_non_generic_descriptor_refuses_base_url_and_header() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "anthropic",
                "--model",
                "claude-x",
                "--base-url",
                "https://api.example.com",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("openai-compatible"), "{stderr}");
    assert!(stderr.contains("--base-url"), "{stderr}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "anthropic",
                "--model",
                "claude-x",
                "--header",
                "x-a=b",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--header"), "{stderr}");
}

/// ADR 0026 D0 acceptance: `--provider opencode-go` sends `x-opencode-session` exactly as the
/// old `--header 'x-opencode-session={session_id}'` did, plus `x-opencode-client: radian`.
#[tokio::test]
async fn opencode_go_sends_the_session_header_byte_for_byte() {
    let server = MockServer::start(vec![text_reply("GO-OK")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--provider",
                "opencode-go",
                "--model",
                "deepseek-v4.1-flash",
                "--session-id",
                "go-session",
                "-p",
                "hi",
            ],
            &[
                ("OPENCODE_GO_API_KEY", "sk-go-test"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("GO-OK"), "{stdout}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].header("x-opencode-session"), Some("go-session"));
    assert_eq!(requests[0].header("x-opencode-client"), Some("radian"));
}

/// ADR 0026 D2 acceptance: a config that still has `model.api_key` is refused, never silently
/// ignored, and the message points at `auth.json`.
#[tokio::test]
async fn a_config_with_model_api_key_is_refused() {
    let dir = scratch("api-key-removed");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        "schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"K\" }\n",
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "-p", "hi"],
            &[("RADIAN_CONFIG", path.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    // The error is the config refusal, not the later no-credential error: it names the key.
    assert!(stderr.contains("model.api_key"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(stderr.contains("/login"), "{stderr}");
}

// ---------- custom headers ----------

#[tokio::test]
async fn a_header_flag_is_sent_and_can_be_repeated() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-opencode-session=stable-123",
                "--header",
                "x-team=radian",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let request = &server.requests()[0];
    assert_eq!(request.header("x-opencode-session"), Some("stable-123"));
    assert_eq!(request.header("x-team"), Some("radian"));
}

#[tokio::test]
async fn a_header_without_an_equals_sign_is_a_usage_error_that_does_not_echo_it() {
    // The malformed argument may itself be a credential: the error must not contain it.
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9",
                "--header",
                "Bearer sk-looks-like-a-secret",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("NAME=VALUE"), "{stderr}");
    assert!(!stderr.contains("sk-looks-like-a-secret"), "{stderr}");
}

#[tokio::test]
async fn an_invalid_header_name_is_refused() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9",
                "--header",
                "bad name=value",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stderr.contains("not a valid HTTP header name"), "{stderr}");
}

#[tokio::test]
async fn an_authorization_header_is_refused_so_the_key_has_one_source() {
    let server = MockServer::start(vec![text_reply("should not be reached")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "Authorization=Bearer sk-from-a-header",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(!stderr.contains("sk-from-a-header"), "{stderr}");
    assert!(
        server.requests().is_empty(),
        "no request may be sent when a header is refused"
    );
}

#[tokio::test]
async fn a_header_value_never_leaks_into_errors() {
    // The server echoes the header value in an error body; neither stream may carry it.
    let server = MockServer::start(vec![Reply::status(
        400,
        "{\"error\":{\"message\":\"bad team-secret-value\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-team-token=team-secret-value",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(!stderr.contains("team-secret-value"), "{stderr}");
}

// ---------- session id and usage ----------

/// The `[session] <id>` note, when the CLI printed one.
fn generated_session_id(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        Some(
            line.strip_prefix("[session] ")?
                .split(' ')
                .next()?
                .to_owned(),
        )
    })
}

#[tokio::test]
async fn a_header_value_can_reference_the_generated_session_id() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--header",
                "x-opencode-session={session_id}",
            ],
            &[],
        ),
        "first\nsecond\n",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let id = generated_session_id(&stderr).unwrap_or_else(|| panic!("no session note: {stderr}"));
    assert!(!id.is_empty());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].header("x-opencode-session"), Some(id.as_str()));
    // The same process uses the same id for every request.
    assert_eq!(requests[1].header("x-opencode-session"), Some(id.as_str()));
}

#[tokio::test]
async fn two_processes_get_different_generated_session_ids() {
    let mut ids = Vec::new();
    for _ in 0..2 {
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--header",
                    "x-session={session_id}",
                    "-p",
                    "hi",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
        let id = generated_session_id(&stderr).expect("the CLI printed the generated id");
        assert_eq!(server.requests()[0].header("x-session"), Some(id.as_str()));
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1], "different processes must differ");
}

#[tokio::test]
async fn an_explicit_session_id_is_used_verbatim_and_not_announced() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "fixed-42",
                "--header",
                "x-session={session_id}",
            ],
            &[],
        ),
        "first\nsecond\n",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        generated_session_id(&stderr).is_none(),
        "an explicit id needs no note: {stderr}"
    );
    let requests = server.requests();
    assert_eq!(requests[0].header("x-session"), Some("fixed-42"));
    assert_eq!(requests[1].header("x-session"), Some("fixed-42"));
}

#[tokio::test]
async fn include_usage_flag_controls_stream_options() {
    // Absent: the default policy. 127.0.0.1 is not api.openai.com, so usage is off.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;
    assert!(server.requests()[0].json().get("stream_options").is_none());

    // --include-usage: on.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--include-usage",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(
        server.requests()[0].json()["stream_options"],
        json!({"include_usage": true})
    );

    // --include-usage=false: explicitly off.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--include-usage=false",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert!(server.requests()[0].json().get("stream_options").is_none());
}

// ---------- tool approval ----------

#[tokio::test]
async fn answering_y_approves_and_the_tool_runs() {
    let dir = scratch("approve-y");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
                // MCP asks under `workspace`; `full` allows it (ADR 0012/0043).
                "--profile",
                "workspace",
            ],
            &[],
        ),
        "go\ny\n/exit\n",
    )
    .await;

    assert_eq!(code, 0);
    assert!(stderr.contains("[y/N]"), "{stderr}");
    assert_eq!(server.requests().len(), 2);
    // The echo fixture returns its arguments; the approval let it run.
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

#[tokio::test]
async fn answering_n_denies_the_call() {
    let dir = scratch("approve-n");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("fine")).await;

    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
                "--profile",
                "workspace",
            ],
            &[],
        ),
        "go\nn\n/exit\n",
    )
    .await;

    assert_eq!(code, 0);
    let denial = tool_result_content(&server, 1);
    assert!(denial.contains("denied"), "{denial}");
    assert!(!denial.contains(r#"{"hello":"mcp"}"#), "{denial}");
}

#[tokio::test]
async fn yes_flag_allows_without_asking() {
    let dir = scratch("yes");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--mcp-config",
                config.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert!(!stderr.contains("[y/N]"), "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

#[tokio::test]
async fn one_shot_without_a_terminal_denies_without_waiting() {
    let dir = scratch("deny");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("fine")).await;

    // stdin is a pipe that stays open and that nobody writes to: the CLI must deny at once
    // because it has no terminal, not because it ran into end of input. Closing stdin here
    // would let an approval that does try to read look the same.
    let running = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--mcp-config",
            config.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    let (code, _, _) = running
        .finish_open(
            EXIT_WATCHDOG,
            "no input was written; the denial must not wait for EOF",
        )
        .await;

    assert_eq!(code, 0);
    let denial = tool_result_content(&server, 1);
    assert!(denial.contains("no terminal"), "{denial}");
    assert!(denial.contains("--yes"), "{denial}");
}

// ---------- interactive mode ----------

#[tokio::test]
async fn interactive_sessions_keep_history_and_eof_exits_cleanly() {
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, stdout, _) = feed_and_wait(
        spawn_radian(&["--model", "m", "--base-url", &server.base_url], &[]),
        "first\nsecond\n", // EOF, no /exit
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "one\ntwo\n");
    let second = messages_of(&server, 1);
    assert_eq!(last_user_text(&second), "second");
    // The first round-trip is still in the conversation.
    let texts: Vec<&str> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect();
    assert!(texts.contains(&"first"), "{second}");
    assert!(texts.contains(&"one"), "{second}");
}

#[tokio::test]
async fn system_prompt_is_sent() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--system",
                "Be brief.",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    assert_eq!(system["role"], "system");
    let content = system["content"].as_str().unwrap_or_default();
    // ADR 0036: `--system` replaces the role layer only. Layer 0a `<runtime>` is
    // first and layer 0b `<session>` is last; the skills section (ADR 0025) sits between them.
    assert!(content.starts_with("<runtime>"), "{content}");
    assert!(
        content.contains("<role>\nBe brief.\n</role>"),
        "the role layer is the --system text: {content}"
    );
    assert!(content.contains("## Skills (ADR 0025)"), "{content}");
    assert!(
        content.trim_end().ends_with("</session>"),
        "layer 0b is last: {content}"
    );
    assert!(content.contains("model: m"), "{content}");
}

/// ADR 0036: without `--system` the built-in `default` role is layer 1, between the
/// `<runtime>` layer and the context files, with the `<session>` layer last.
#[tokio::test]
async fn the_default_role_layers_are_sent() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    // A private home: the mounted tools must not depend on the machine's real memory.
    let home = scratch("default-role");
    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0);
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    assert!(content.starts_with("<runtime>"), "{content}");
    let runtime_at = content.find("<runtime>").unwrap();
    let role_at = content.find("<role>").unwrap();
    let session_at = content.find("<session>").unwrap();
    assert!(runtime_at < role_at && role_at < session_at, "{content}");
    assert!(
        content.contains("You are radian, a coding agent"),
        "the default role: {content}"
    );
    // ADR 0021 D7: an empty memory mounts no search, so the tool definitions name no
    // memory tool at all.
    assert!(
        !content.contains("memory_search"),
        "the mounted tools: {content}"
    );
    assert!(content.trim_end().ends_with("</session>"), "{content}");
    assert!(content.contains("model: m"), "{content}");
}

/// The `<runtime>` Unix-socket sentence follows the backend: Linux bwrap denies every
/// Unix socket in `workspace` (slice 0's seccomp filter), so the sentence is present;
/// macOS seatbelt does too, so the sentence stays exactly as today. Setting the flag
/// from the tier again fails the Linux half.
#[tokio::test]
async fn the_workspace_system_prompt_unix_socket_sentence_follows_the_backend() {
    let dir = scratch("unix-sockets-backend");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    if cfg!(target_os = "linux") {
        assert!(
            content.contains("Outbound Unix socket connections are denied"),
            "bwrap denies every Unix socket in workspace, so the sentence is present: {content}"
        );
    } else {
        assert!(
            content.contains("Outbound Unix socket connections are denied"),
            "seatbelt restricts Unix sockets, so the sentence stays: {content}"
        );
    }
}

/// The `full` tier has no sandbox and a reachable network (ADR 0016 decision 1, ADR
/// 0043 decision 5), so its `<runtime>` facts say so in plain sentences.
#[tokio::test]
async fn the_full_system_prompt_states_network_on() {
    let dir = scratch("full-network-on");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let messages = messages_of(&server, 0);
    let system = &messages.as_array().expect("messages")[0];
    let content = system["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("There is no sandbox."),
        "full has no sandbox: {content}"
    );
    assert!(
        content.contains("The network is on."),
        "full has a reachable network: {content}"
    );
    assert!(
        !content.contains("Network: off."),
        "no stale off flag beside the no-sandbox facts: {content}"
    );
}

/// The removed tier name is unknown: `--profile safe` fails like any typo (fail closed),
/// and never runs the model.
#[tokio::test]
async fn profile_safe_is_rejected_as_an_unknown_tier() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--profile",
                "safe",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_ne!(code, 0, "an unknown tier must fail: {stderr}");
    assert!(
        stderr.contains("'safe'"),
        "the refusal must name the problem: {stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "the model must never run for an unknown tier"
    );
}

#[tokio::test]
async fn turn_timeout_ends_the_turn() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--turn-timeout-secs",
                "1",
            ],
            &[],
        ),
        "",
    )
    .await;

    // A turn that hits the deadline ends Cancelled(DeadlineExceeded): exit 3 (ADR 0044).
    assert_eq!(code, 3, "a deadline-exceeded turn exits 3: {stderr}");
    assert!(stderr.contains("DeadlineExceeded"), "{stderr}");
    assert!(
        start.elapsed() < Duration::from_secs(6),
        "{:?}",
        start.elapsed()
    );
}

// ---------- Ctrl-C ----------

#[tokio::test]
async fn first_sigint_cancels_the_turn_and_the_session_survives() {
    let server = MockServer::start(vec![
        Reply::SseThenHang { pieces: Vec::new() },
        text_reply("recovered"),
    ])
    .await;
    let mut child = Running::spawn(&["--model", "m", "--base-url", &server.base_url], &[]);

    child.write("go\n").await;
    // The recorded request proves the turn is hanging on the model, and the interrupt
    // task has been waiting since before that — a signal now cannot land at the prompt.
    wait_until("the model request", || server.requests().len() == 1).await;
    sigint(&child.child);
    child.write("again\n/exit\n").await;

    let (code, stdout, stderr) = child.finish().await;

    assert_eq!(code, 0);
    assert!(stderr.contains("cancelled"), "{stderr}");
    // The second turn really ran, on the same session.
    assert_eq!(server.requests().len(), 2);
    assert_eq!(stdout, "recovered\n");
    assert_eq!(last_user_text(&messages_of(&server, 1)), "again");
}

#[tokio::test]
async fn second_sigint_exits_130() {
    // Load note: the two signals are separated only by a pipe round-trip (the marker
    // wait below), so this test is the load detector for the signal path. A listener
    // that re-subscribes after every signal loses the second Ctrl-C when the
    // forwarding task is preempted in the gap — the turn still ends Cancelled and the
    // loop idles forever — about one hung iteration in seven with six busy loops and
    // this test pinned to the same four cores. Validate signal-handling changes with
    // that pinning and `--stress-count 300`: the fixed listener trips zero.
    let dir = scratch("130");
    let log = dir.join("fixture.log");
    let config = mcp_config(&dir, &log);
    // `freeze` blocks the fixture's reading thread, so after the first Ctrl-C the client
    // waits out its full 2s MCP cancel grace for a ping that can never be answered: a real,
    // bounded wind-down window for the second Ctrl-C.
    let server = MockServer::start(vec![tool_reply("call_1", "fx__freeze", "{}")]).await;
    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--yes",
            "--mcp-config",
            config.to_str().unwrap(),
        ],
        &[],
    );
    let start = Instant::now();

    child.write("go\n").await;
    // The fixture log proves the tool call is active before the first signal.
    wait_until("the fixture to receive tools/call", || {
        std::fs::read_to_string(&log)
            .map(|text| text.contains("tools/call"))
            .unwrap_or(false)
    })
    .await;
    sigint(&child.child);
    // State gate, not a timer: wait until the CLI reports the wind-down, which proves the
    // first Ctrl-C was handled while the turn is still winding down (the MCP cancel grace
    // keeps it there), then send the second one.
    child.wait_for_stderr("Ctrl-C again to quit").await;
    let forced_at = Instant::now();
    sigint(&child.child);

    // Keep stdin open while waiting: closing it would race an EOF against the second signal,
    // and this test is about the signal, not about EOF.
    let (code, _, stderr) = child
        .finish_open(
            EXIT_WATCHDOG,
            "the second SIGINT was already sent after the wind-down marker",
        )
        .await;

    assert_eq!(code, 130, "{stderr}");
    // The second signal forces the exit; it does not wait out the MCP cancel grace (2s).
    assert!(
        forced_at.elapsed() < Duration::from_millis(1000),
        "the forced exit waited instead of forcing: {:?}",
        forced_at.elapsed()
    );
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "{:?}",
        start.elapsed()
    );
    // Even the forced exit reaped the fixture.
    assert!(gone_within(fixture_pid(&log), STATE_WATCHDOG).await);
}

// ---------- child process lifetime ----------

#[tokio::test]
async fn every_exit_reaps_the_mcp_children() {
    let dir = scratch("reap");
    for (name, input, sigint_at_prompt) in [
        ("exit", "hi\n/exit\n", false),
        ("eof", "hi\n", false),
        ("sigint", "", true),
    ] {
        let log = dir.join(format!("{name}.log"));
        // This fixture does not leave when its stdin closes. A server that did would vanish
        // when the CLI process dies, whatever the CLI did, and the test could not tell.
        let config = mcp_config_with(&dir, &log, &[("FIXTURE_IGNORE_EOF", "1")], Some(1));
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let args = [
            "--model".to_owned(),
            "m".to_owned(),
            "--base-url".to_owned(),
            server.base_url.clone(),
            "--mcp-config".to_owned(),
            config.display().to_string(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _) = if sigint_at_prompt {
            // Wait until the prompt is really up, then leave with Ctrl-C.
            let mut running = Running::spawn(&arg_refs, &[]);
            running.wait_for_stderr("> ").await;
            sigint(&running.child);
            let (code, _, _) = running.finish().await;
            (code, ())
        } else {
            let (code, _, _) = feed_and_wait(spawn_radian(&arg_refs, &[]), input).await;
            (code, ())
        };
        assert_eq!(code, 0, "{name}");

        let pid = fixture_pid(&log);
        assert!(
            gone_within(pid, STATE_WATCHDOG).await,
            "{name}: fixture {pid} still running"
        );
    }
}

#[tokio::test]
async fn a_normal_exit_lets_each_server_shut_down_before_the_cli_leaves() {
    let dir = scratch("graceful");
    for (name, input) in [("exit", "hi\n/exit\n"), ("eof", "hi\n")] {
        let log = dir.join(format!("{name}.log"));
        // The server takes 300 ms to finish after its stdin closes. If the CLI killed it, or
        // just left, the marker would be missing (or arrive after the CLI is gone).
        let config = mcp_config_with(&dir, &log, &[("FIXTURE_SLOW_SHUTDOWN_MS", "300")], None);
        let server = MockServer::start(vec![text_reply("ok")]).await;
        let args = [
            "--model".to_owned(),
            "m".to_owned(),
            "--base-url".to_owned(),
            server.base_url.clone(),
            "--mcp-config".to_owned(),
            config.display().to_string(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

        let (code, _, _) = feed_and_wait(spawn_radian(&arg_refs, &[]), input).await;

        assert_eq!(code, 0, "{name}");
        let lines = log_lines(&log);
        assert!(
            lines.contains(&"shutdown-complete".to_owned()),
            "{name}: {lines:?}"
        );
    }
}

#[tokio::test]
async fn typing_exit_leaves_even_though_stdin_is_still_open() {
    // A terminal never reaches end of input. The CLI must not wait for one more line after
    // `/exit`: the stdin reader's blocking read cannot be interrupted, so exiting has to
    // abandon it instead of waiting for it.
    let server = MockServer::start(vec![text_reply("hello")]).await;
    let mut running = Running::spawn(&["--model", "m", "--base-url", &server.base_url], &[]);

    running.write("hi\n/exit\n").await;
    let (code, stdout, _) = running
        .finish_open(EXIT_WATCHDOG, "/exit was written; stdin stayed open")
        .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "hello\n");
}

// ---------- approval must not eat input ----------

#[tokio::test]
async fn cancelling_at_the_approval_prompt_does_not_swallow_the_next_line() {
    let dir = scratch("swallow");
    let config = mcp_config(&dir, &dir.join("fixture.log"));
    let server = MockServer::start(echo_then("ok")).await;
    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--mcp-config",
            config.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );

    child.write("go\n").await;
    // The request alone is not enough: it was sent before `before_tool_call`. The approval
    // prompt on stderr proves the turn is really waiting for an answer.
    wait_until("the tool request", || server.requests().len() == 1).await;
    child.wait_for_stderr("[y/N]").await;
    sigint(&child.child);
    // Wait until the CLI has actually cancelled before typing: the next line must belong
    // to the next prompt, not race the dying approval for it.
    child.wait_for_stderr("Cancelled").await;
    child.write("next\n/exit\n").await;

    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    // The line after the cancelled turn reached the model, not a stranded read.
    assert_eq!(server.requests().len(), 2, "{stderr}");
    assert_eq!(last_user_text(&messages_of(&server, 1)), "next", "{stderr}");
}

// ---------- built-in coding tools ----------

/// One-shot: the model asks for the built-in `write`, then answers.
#[tokio::test]
async fn built_in_write_tool_creates_a_file_in_the_workspace() {
    let dir = scratch("builtin-write");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "write",
            r#"{"path":"out.txt","content":"hello\n"}"#,
        ),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "write a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap(),
        "hello\n"
    );
    // The tool result went back to the model.
    assert!(
        tool_result_content(&server, 1).contains("wrote 6 bytes"),
        "{}",
        tool_result_content(&server, 1)
    );
}

#[tokio::test]
async fn built_in_read_outside_the_workspace_is_refused_and_the_turn_continues() {
    let dir = scratch("builtin-escape");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("call_1", "read", r#"{"path":"/etc/passwd"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "read a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "the turn continues: {stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("absolute path") && result.contains("/etc/passwd"),
        "the model sees the refusal: {result}"
    );
}

/// ADR 0039 decision 4: the `full` tier reads an outside path through the file tools. The
/// model reads a scratch file outside the workspace by absolute path, and the turn
/// continues with its content.
#[tokio::test]
async fn built_in_read_outside_the_workspace_reads_in_the_full_tier() {
    let dir = scratch("builtin-full-read");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let outside = dir.join("outside.txt");
    std::fs::write(&outside, "FULL-TIER-SENTINEL\n").unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "call_1",
            "read",
            &format!(
                r#"{{"path":{}}}"#,
                serde_json::to_string(outside.to_str().unwrap()).unwrap()
            ),
        ),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "read a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "the turn continues: {stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("FULL-TIER-SENTINEL"),
        "the model sees the outside file: {result}"
    );
}

/// ADR 0039 decision 4 end to end: a scripted model `write`s a memory file and `read`s it
/// back in `radian exec --profile workspace`. The journal settles both calls without error,
/// and the file on disk carries the bytes — the file tools' half of the memory opening.
#[tokio::test]
async fn a_scripted_model_writes_and_reads_a_memory_file_in_the_workspace_tier() {
    // `common::process_dir`, not `scratch`: this test never binds its own Unix socket, and the
    // system temp dir keeps it runnable inside the `workspace` sandbox too (a `/tmp` scratch
    // is refused there). The session's control socket may fail to bind on a long macOS temp
    // path; that is a warning, never a startup refusal (ADR 0043).
    let dir = common::process_dir("radian-exec-memory-roots");
    let ws = dir.join("ws");
    let home = dir.join("radian");
    let sessions = dir.join("sessions");
    for path in [&ws, &home, &sessions] {
        std::fs::create_dir_all(path).unwrap();
    }
    // The host creates `memory/global/` when it builds the file tools; the model writes a
    // file straight into it, with no memory command in between.
    let memory_file = home.join("memory/global/e2e.md");
    let target = serde_json::to_string(memory_file.to_str().unwrap()).unwrap();

    let server = MockServer::start(vec![
        tool_reply(
            "w1",
            "write",
            &format!("{{\"path\":{target},\"content\":\"E2E-MEMORY\\n\"}}"),
        ),
        tool_reply("r1", "read", &format!("{{\"path\":{target}}}")),
        text_reply("done"),
    ])
    .await;

    let envs = [
        ("RADIAN_HOME", home.to_str().unwrap()),
        ("RADIAN_SESSION_DIR", sessions.to_str().unwrap()),
    ];
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--session-id",
                "memory-files",
                "--yes",
                "-p",
                "write and read a memory file",
            ],
            &envs,
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&memory_file).unwrap(),
        "E2E-MEMORY\n",
        "the memory file was written where the model named it"
    );

    // Both tool calls settled without error in the journal, and the read returned the bytes
    // the write had just written.
    let raw = std::fs::read_to_string(sessions.join("memory-files.jsonl")).unwrap();
    let settled: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["event"] == "ToolExecutionSettled")
        .map(|value| value["data"].clone())
        .collect();
    let settled_for = |call: &str| -> Value {
        settled
            .iter()
            .find(|data| data["call_id"] == call)
            .unwrap_or_else(|| panic!("no settled event for {call}: {settled:#?}"))
            .clone()
    };
    assert_eq!(settled.len(), 2, "one settled event per call: {settled:#?}");
    for call in ["w1", "r1"] {
        assert_eq!(
            settled_for(call)["result"]["is_error"],
            false,
            "{call} succeeded: {}",
            settled_for(call)
        );
    }
    assert!(
        settled_for("r1").to_string().contains("E2E-MEMORY"),
        "the read returned the written bytes: {}",
        settled_for("r1")
    );
}

#[tokio::test]
async fn without_workspace_the_built_in_tools_do_not_exist() {
    let server = MockServer::start(vec![
        tool_reply("call_1", "write", r#"{"path":"x","content":"y"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "-p",
                "write a file",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("no tool named"),
        "without --workspace there is no `write`: {result}"
    );
}

// ---------- sessions on disk (Phase 2 / B) ----------

/// 1: a new session writes a valid, contiguous JSONL with the runtime's events.
#[tokio::test]
async fn a_new_session_writes_a_valid_jsonl() {
    let sessions = fresh_session_dir();
    let server = MockServer::start(vec![text_reply("hi")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-one",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hello",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let path = sessions.join("s-one.jsonl");
    let session = radian_session::Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (0..seqs.len() as u64).collect::<Vec<_>>(),
        "seq is contiguous"
    );
    for event in [
        "SessionCreated",
        "InboxEnqueued",
        "InputClaimed",
        "InputAdmitted",
        "TurnFinished",
    ] {
        assert!(
            session.events().iter().any(|e| e.event == event),
            "missing {event} in {:?}",
            session
                .events()
                .iter()
                .map(|e| &e.event)
                .collect::<Vec<_>>()
        );
    }
    // The one-shot turn is on disk, not only in memory.
    assert_eq!(session.history().unwrap().len(), 2);
    // The terminal input is recorded as human input (ADR 0011 L2): the permission layer
    // relies on this value, so it must not be `None` or some other source.
    let enqueued = session
        .events()
        .iter()
        .find(|e| e.event == "InboxEnqueued")
        .expect("the input was enqueued");
    match &enqueued.data {
        radian_session::EventData::InboxEnqueued { source, .. } => {
            assert_eq!(*source, radian_core::InputSource::User)
        }
        other => panic!("expected InboxEnqueued, got {other:?}"),
    }
}

/// 2: `--resume` puts the previous history in front of the new request.
#[tokio::test]
async fn resume_carries_the_previous_history() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-resume",
                "--session-dir",
                dir,
                "-p",
                "first",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--resume",
                "s-resume",
                "--session-dir",
                dir,
                "-p",
                "second",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let second = messages_of(&server, 1);
    assert_eq!(last_user_text(&second), "second");
    let texts: Vec<&str> = second
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect();
    assert!(
        texts.contains(&"first"),
        "the resumed request lacks the old input: {second}"
    );
    assert!(
        texts.contains(&"one"),
        "the resumed request lacks the old answer: {second}"
    );
}

/// 4: `--list` shows only the sessions of the current workspace.
#[tokio::test]
async fn list_shows_only_the_current_workspace() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    // The workspaces must be siblings of the session directory: a workspace inside it is
    // refused (ADR 0020), and this test is about `--list`, not the sandbox.
    let name = sessions.file_name().unwrap().to_string_lossy().into_owned();
    let ws_a = sessions.with_file_name(format!("{name}-ws-a"));
    let ws_b = sessions.with_file_name(format!("{name}-ws-b"));
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let server = MockServer::start(vec![text_reply("a"), text_reply("b")]).await;

    for (id, workspace) in [("a-1", &ws_a), ("b-1", &ws_b)] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    workspace.to_str().unwrap(),
                    "--session-id",
                    id,
                    "--session-dir",
                    dir,
                    "-p",
                    "hi",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
    }

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--workspace",
                ws_a.to_str().unwrap(),
                "--session-dir",
                dir,
                "--list",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("a-1"), "{stdout}");
    assert!(
        !stdout.contains("b-1"),
        "another workspace leaked in: {stdout}"
    );
    // The row carries id, created, last activity and a message count.
    let row = stdout.lines().find(|l| l.contains("a-1")).unwrap();
    assert_eq!(row.split('\t').count(), 4, "{row}");
    assert_eq!(row.split('\t').nth(3).unwrap(), "2", "{row}");
}

/// 5: reasoning is printed apart from the visible answer.
#[tokio::test]
async fn reasoning_is_printed_apart_from_the_answer() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            json!({"role": "assistant", "reasoning_content": "thinking hard"}),
            None,
        ),
        chunk(json!({"content": "the answer"}), None),
        chunk(json!({}), Some("stop")),
    ])])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "the answer\n", "stdout carries only the answer");
    assert!(stderr.contains("[reasoning]"), "{stderr}");
    assert!(stderr.contains("thinking hard"), "{stderr}");
    assert!(
        !stdout.contains("thinking hard"),
        "reasoning leaked to stdout"
    );
}

/// 6: a second process cannot open a session another process holds.
#[tokio::test]
async fn a_second_process_cannot_open_the_same_session() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;

    let mut first = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "s-lock",
            "--session-dir",
            dir,
        ],
        &[],
    );
    first.write("go\n").await;
    wait_until("the model request", || server.requests().len() == 1).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--resume",
                "s-lock",
                "--session-dir",
                dir,
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("locked") || stderr.contains("another writer"),
        "{stderr}"
    );

    // Release the first process and let it leave.
    sigint(&first.child);
    first.wait_for_stderr("Cancelled").await;
    first.write("/exit\n").await;
    let (code, _, _) = first.finish().await;
    assert_eq!(code, 0);
}

/// 7: the session file carries neither the API key nor a header value.
#[tokio::test]
async fn the_session_file_contains_no_key_or_header_values() {
    let sessions = fresh_session_dir();
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-secret",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--header",
                "x-team-token=file-secret-value",
                "-p",
                "hi",
            ],
            &[("RADIAN_EXEC_FILE_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let body = std::fs::read_to_string(sessions.join("s-secret.jsonl")).unwrap();
    assert!(!body.contains(KEY), "the key reached the session file");
    assert!(
        !body.contains("file-secret-value"),
        "a header value reached the session file"
    );
    assert!(!body.contains("Authorization"), "{body}");
}

/// 3: the process is killed mid-turn (`kill -9`), and `--resume` recovers: the unpaired tool
/// is classified and **not** replayed, and the input that was claimed but never admitted
/// comes back instead of being lost.
#[tokio::test]
async fn a_killed_session_resumes_without_replaying_the_tool() {
    let sessions = fresh_session_dir();
    let dir = sessions.to_str().unwrap();
    let scratch = scratch("kill-resume");
    let log = scratch.join("fixture.log");
    let config = mcp_config(&scratch, &log);
    let path = sessions.join("s-kill.jsonl");
    // `fx__sleep` ignores cancellation, so the turn is genuinely mid-tool when the process
    // dies; the fixture has no `FIXTURE_IGNORE_EOF`, so killing the CLI releases it.
    // The resumed session has a pending steer, which (ADR 0011) opens a turn by
    // itself as soon as the driver runs — before the `-p next` followup. The three replies
    // cover the killed turn's tool call and whichever of the resumed turns runs first.
    let server = MockServer::start(vec![
        tool_reply("call_1", "fx__sleep", "{\"ms\": 30000}"),
        text_reply("done"),
        text_reply("done"),
    ])
    .await;

    let mut first = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--yes",
            "--mcp-config",
            config.to_str().unwrap(),
            "--session-id",
            "s-kill",
            "--session-dir",
            dir,
        ],
        &[],
    );
    first.write("go\n").await;
    wait_until("the tool call to start", || {
        std::fs::read_to_string(&log)
            .map(|text| text.contains("tools/call"))
            .unwrap_or(false)
    })
    .await;
    // With `--yes` the loop reads during the turn, so this line becomes a steer. Wait until
    // it is durably enqueued (a second InboxEnqueued) before killing. The claim now happens
    // at the step boundary — the driver's wake branch only handles cancellation — so a steer
    // that arrives mid-step is requeued from its InboxEnqueued record on recovery.
    first.write("steer-line\n").await;
    wait_until("the steer to be enqueued", || {
        std::fs::read_to_string(&path)
            .map(|text| text.matches("\"InboxEnqueued\"").count() >= 2)
            .unwrap_or(false)
    })
    .await;

    // The genuine crash: SIGKILL, no chance to flush anything.
    first.child.kill().await.unwrap();
    let _ = first.child.wait().await;
    let pid = fixture_pid(&log);
    assert!(
        gone_within(pid, STATE_WATCHDOG).await,
        "the fixture survived the killed CLI"
    );

    // What the crash left: the tool started, never settled.
    let crashed = std::fs::read_to_string(&path).unwrap();
    assert!(crashed.contains("\"ToolExecutionStarted\""), "{crashed}");
    assert!(!crashed.contains("\"ToolExecutionSettled\""), "{crashed}");
    assert_eq!(
        crashed.matches("tools/call").count(),
        0,
        "the session file must not contain fixture logs"
    );
    let calls_before = log_lines(&log)
        .iter()
        .filter(|line| line.contains("tools/call"))
        .count();
    assert_eq!(calls_before, 1);

    // Resume: the repaired result is written back, the steer is requeued and applied, and
    // the tool is not run again.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "--mcp-config",
                config.to_str().unwrap(),
                "--resume",
                "s-kill",
                "--session-dir",
                dir,
                "-p",
                "next",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let session = radian_session::Session::open(&path).unwrap();
    let history = session.history().unwrap();
    let texts: Vec<String> = history.iter().map(radian_core::Message::text).collect();
    assert!(
        texts.contains(&"steer-line".to_owned()),
        "the claimed steer was lost: {texts:?}"
    );
    let repaired = history.iter().find_map(|m| match m {
        radian_core::Message::Tool { result, .. } if result.call_id == "call_1" => Some(result),
        _ => None,
    });
    assert_eq!(
        repaired.and_then(|r| r.error_code.as_deref()),
        Some("outcome_unknown_after_crash"),
        "the unpaired call was not classified: {history:?}"
    );
    // The tool was not replayed: still exactly one fixture call, in the first process.
    let calls_after = log_lines(&log)
        .iter()
        .filter(|line| line.contains("tools/call"))
        .count();
    assert_eq!(calls_after, 1, "the sleep was replayed on resume");
    // The steer was admitted in the resumed turn, not merely left in the queue.
    let events = std::fs::read_to_string(&path).unwrap();
    assert!(
        events.matches("\"InputAdmitted\"").count() >= 2,
        "the requeued steer was never admitted: {events}"
    );
}

// ---------- profiles, the sandbox, and the permission layer (Phase 3 / cli-tools) ----------

/// The tool names the adapter advertised in request `n`.
fn advertised_tools(server: &MockServer, n: usize) -> Vec<String> {
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

/// 1 (ADR 0043): `full` **is** the default, and without `--workspace` no write/execute tool
/// exists at all. The startup line says `no sandbox`; putting the default back to `workspace`
/// is the mutation this catches.
#[tokio::test]
async fn full_is_the_default_and_no_write_tools_without_a_workspace() {
    let server = MockServer::start(vec![
        tool_reply("c1", "write", r#"{"path":"x.txt","content":"y"}"#),
        tool_reply("c2", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--yes",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    // The default is named on startup, and it is `full` with no sandbox and no prompts.
    assert!(stderr.contains("[profile] full"), "{stderr}");
    assert!(
        stderr.contains("no prompts"),
        "the default must say nothing prompts: {stderr}"
    );
    assert!(
        stderr.contains("[sandbox] no sandbox:"),
        "the default must say there is no sandbox: {stderr}"
    );
    // Neither tool is advertised, so the model cannot even ask for one.
    let names = advertised_tools(&server, 0);
    for absent in ["write", "patch", "shell"] {
        assert!(
            !names.contains(&absent.to_owned()),
            "`{absent}` must not exist without --workspace: {names:?}"
        );
    }
    assert!(tool_result_content(&server, 1).contains("no tool named"));
    assert!(tool_result_content(&server, 2).contains("no tool named"));
}

/// ADR 0043 decision 2: `--profile workspace` names the enforced tier, and the startup line
/// says what it means ("sandbox on: commands stay in the workspace").
#[tokio::test]
async fn the_workspace_flag_turns_the_sandbox_on() {
    let dir = scratch("flag-workspace");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("sandbox on: commands stay in the workspace"),
        "{stderr}"
    );
}

/// ADR 0043 §4 (TK0216): a request carries only mounted schemas, and a description names
/// another tool only when that tool is mounted. The Terminal-Bench arm mounts
/// `read,write,patch,shell`; `grep`, `glob` and `job` are absent, so the shell text must not
/// name them.
#[tokio::test]
async fn a_four_tool_set_keeps_the_shell_description_inside_the_set() {
    // `common::process_dir` stays inside the workspace sandbox's writable tree, so this test
    // also runs under the `sandbox` nextest profile (no `/tmp`). `full` is the default and
    // needs no backend, so no nested Seatbelt is asked for.
    let dir = common::process_dir("radian-exec-tools-shell");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--tools",
                "read,write,patch,shell",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        advertised_tools(&server, 0),
        ["read", "write", "patch", "shell"],
        "the reduced arm mounts exactly its four tools"
    );
    let tools = server.requests()[0].json()["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let shell = tools
        .iter()
        .find(|tool| tool["function"]["name"] == "shell")
        .expect("shell is mounted");
    let description = shell["function"]["description"]
        .as_str()
        .expect("the shell description");
    for absent in ["`grep`", "`glob`", "`find`", "search files"] {
        assert!(
            !description.contains(absent),
            "{absent:?} must not be named without its tool: {description}"
        );
    }
    assert!(
        description.contains("Use `read` to read and list files"),
        "{description}"
    );
    let background = shell["function"]["parameters"]["properties"]["background"]["description"]
        .as_str()
        .expect("the background field");
    assert!(
        !background.contains("`job`"),
        "`job` is not mounted, so the background note must not name it: {background}"
    );
}

/// Writes `<ws>/.radian/config.toml` with `text`.
fn write_project_config(ws: &Path, text: &str) {
    let dir = ws.join(".radian");
    std::fs::create_dir_all(&dir).unwrap();
    write_file(&dir, "config.toml", text);
}

/// ADR 0043 decision 2: a project may only narrow, so `full` there is refused with a warning.
/// The mutation this catches is accepting it: with a config `workspace` and a project `full`,
/// the wider tier would take effect.
#[tokio::test]
async fn a_project_config_cannot_widen_the_tier() {
    let dir = scratch("project-full");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"full\"\n");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[permissions]\nprofile = \"workspace\"\n",
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[project]"), "{stderr}");
    assert!(stderr.contains("cannot widen"), "{stderr}");
    // Still the config's `workspace`, not the project's `full`.
    assert!(
        stderr.contains("sandbox on: commands stay in the workspace"),
        "{stderr}"
    );
}

/// ADR 0043 decision 2: the remembered user record and the project declaration are resolved to
/// the **narrower** of the two. Taking the wider one is the mutation this catches.
#[tokio::test]
async fn a_remembered_tier_is_narrowed_by_the_project() {
    let dir = scratch("project-narrows");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"workspace\"\n");
    let rh = dir.join("rh");
    let access = rh.join("access");
    std::fs::create_dir_all(&access).unwrap();
    let key = std::fs::canonicalize(&ws).unwrap();
    write_file(
        &access,
        "access.json",
        &format!("{{\"{}\": \"full\"}}\n", key.display()),
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[("RADIAN_HOME", rh.to_str().unwrap())],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[access] workspace (from project config)"),
        "{stderr}"
    );
    assert!(stderr.contains("[profile] workspace"), "{stderr}");
}

/// ADR 0043 decision 2: an explicit `--profile` wins, and the startup line still names what
/// the project asks for.
#[tokio::test]
async fn an_explicit_flag_wins_and_the_line_names_the_project_ask() {
    let dir = scratch("flag-wins");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_project_config(&ws, "[permissions]\nprofile = \"workspace\"\n");
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[profile] full"), "{stderr}");
    assert!(stderr.contains("[sandbox] no sandbox:"), "{stderr}");
    assert!(
        stderr.contains("the project asks for workspace"),
        "{stderr}"
    );
}

/// ADR 0034: under `full` the shell keeps the six `HERDR_*` names and
/// any `[sandbox] allow_env` name, and still drops everything else. `HERDR_PANE_ID` being set
/// in the parent is what the shell must see.
#[tokio::test]
async fn the_shell_environment_carries_herdr_and_allow_env() {
    let dir = scratch("shell-env");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let rh = dir.join("rh");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[sandbox]\nallow_env = [\"FOO\"]\n",
    );
    let command =
        "printf '%s|%s|%s' \"$HERDR_PANE_ID\" \"$FOO\" \"$SECRET_API_KEY\" > env.txt".to_string();
    let server = MockServer::start(vec![shell_reply("c1", &command), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[
                ("RADIAN_HOME", rh.to_str().unwrap()),
                ("HERDR_PANE_ID", "pane-9"),
                ("FOO", "foo-value"),
                ("SECRET_API_KEY", "sk-secret-value"),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let seen = std::fs::read_to_string(ws.join("env.txt"))
        .unwrap_or_else(|e| panic!("the shell did not run: {e}\n{stderr}"));
    assert_eq!(
        seen, "pane-9|foo-value|",
        "the shell must see the HERDR name and the configured name, and nothing else"
    );
}

/// 2: a failed sandbox probe refuses to run; it never falls back to an unsandboxed run.
///
/// `RADIAN_SANDBOX_PROBE_FAIL=1` is a test seam that makes the CLI behave exactly as if the
/// backend probe had failed. It can only cause a refusal, so it cannot weaken the guarantee;
/// it exists because a real probe failure cannot be produced on a machine that has the
/// backend.
#[tokio::test]
async fn a_failed_sandbox_probe_refuses_instead_of_degrading() {
    let dir = scratch("probe-fail");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                // The enforced tier is what an unavailable backend refuses (ADR 0043:
                // `full` is the default and needs no backend).
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "hi",
            ],
            &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("sandbox backend is unavailable") || stderr.contains("cannot be enforced"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), 0, "the model must never run");
    assert!(stdout.is_empty(), "{stdout}");
    // Nothing was written into the workspace either.
    assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
}

/// 10: the startup text states the real limits and never claims credential isolation
/// (ADR 0012 P4). `--profile workspace` names the tier whose facts sentence carries
/// the limits.
#[tokio::test]
async fn the_startup_text_does_not_claim_credential_isolation() {
    let dir = scratch("wording");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[profile] workspace"), "{stderr}");
    assert!(stderr.contains("[sandbox]"), "{stderr}");
    for phrase in [
        "credentials are isolated",
        "credential isolation",
        "credentials are safe",
        "fully isolated",
        "凭据已隔离",
        "IPC is contained",
    ] {
        assert!(
            !stderr.contains(phrase),
            "the startup text must not claim {phrase:?}: {stderr}"
        );
    }
    // The honest limit is stated instead.
    assert!(stderr.contains("not tightened"), "{stderr}");
    // ADR 0012, decision (option B): MCP servers keep running unsandboxed
    // under workspace-write, and the notice is what tells the user so. With that choice this
    // sentence is the only safeguard, so a test has to hold it in place.
    assert!(
        stderr.contains("MCP servers are NOT sandboxed"),
        "the notice must say MCP runs outside the sandbox: {stderr}"
    );
    // ...and must not claim, unqualified, that every write is confined.
    assert!(
        !stderr.contains("[sandbox] writes are confined to the workspace"),
        "{stderr}"
    );
}

/// ADR 0036: `agent` reads the workspace's `AGENTS.md` and names it on the startup line.
#[tokio::test]
async fn the_context_file_is_reported_on_the_startup_line() {
    let dir = scratch("context-line");
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "CONTEXT-LINE-MARKER\n").unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[context] AGENTS.md"), "{stderr}");
}

/// Every tool result text in request `n` (a batch has more than one).
fn tool_result_contents(server: &MockServer, n: usize) -> Vec<String> {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The tool result text for one call id in request `n`.
fn tool_result_for(server: &MockServer, n: usize, call_id: &str) -> String {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .unwrap_or_else(|| {
            panic!(
                "no tool result for {call_id} in request {n}: {:?}",
                messages_of(server, n)
            )
        })
}

fn shell_reply(id: &str, command: &str) -> Reply {
    tool_reply(id, "shell", &json!({"command": command}).to_string())
}

/// A shell call that asks to leave the sandbox: the ask-mechanics tests below use these,
/// because slice 1 of `sandbox-auto-allow-shell` lets a sandboxed `workspace` shell run
/// without asking. An approved outside call really runs unsandboxed, so these stay on
/// harmless `echo` commands.
fn outside_reply(id: &str, command: &str) -> Reply {
    tool_reply(
        id,
        "shell",
        &json!({
            "command": command,
            "outside_sandbox": true,
            "outside_sandbox_reason": "the test leaves the sandbox on purpose",
        })
        .to_string(),
    )
}

/// 3: a shell call that leaves the sandbox asks; "allow once" runs it; the *next*
/// identical call asks again. Runs under `--profile workspace` with `outside_sandbox`
/// calls — a sandboxed shell no longer asks (slice 1 of `sandbox-auto-allow-shell`).
#[tokio::test]
async fn a_shell_command_asks_every_time_when_allowed_once() {
    let dir = scratch("shell-ask");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        outside_reply("c1", "echo hi"),
        outside_reply("c2", "echo hi"),
        text_reply("done"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("y\n").await;
    child.wait_for_stderr_count("[y/N]", 2).await;
    child.write("y\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("[y/N]").count(),
        2,
        "an allow-once must not cover the next call: {stderr}"
    );
    assert!(
        stderr.contains("echo hi"),
        "the prompt shows the command: {stderr}"
    );
    assert!(tool_result_content(&server, 1).contains("hi"));
    assert!(tool_result_content(&server, 2).contains("hi"));
}

/// 4: "allow for this session" covers the next call in the same turn; a resumed session has
/// no grant and asks again. Uses `outside_sandbox` calls: a sandboxed `workspace` shell
/// no longer asks (slice 1 of `sandbox-auto-allow-shell`), and the grant never crosses
/// the sandbox boundary.
#[tokio::test]
async fn allow_for_session_covers_the_turn_and_not_a_resume() {
    let sessions = fresh_session_dir();
    let dir = scratch("shell-session");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let outside_args = json!({
        "command": "echo hi",
        "outside_sandbox": true,
        "outside_sandbox_reason": "the test leaves the sandbox on purpose",
    })
    .to_string();
    let two_calls = Reply::sse(&[
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "c1", "type": "function",
                 "function": {"name": "shell", "arguments": outside_args}},
                {"index": 1, "id": "c2", "type": "function",
                 "function": {"name": "shell", "arguments": outside_args.clone()}}
            ]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let server = MockServer::start(vec![
        two_calls,
        text_reply("done"),
        outside_reply("c3", "echo hi"),
        text_reply("done again"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-session",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("a\n").await; // always for this session
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("[y/N]").count(),
        1,
        "the second call in the same turn must use the grant: {stderr}"
    );
    let results = tool_result_contents(&server, 1);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results.iter().all(|text| text.contains("hi")),
        "{results:?}"
    );

    // Resume: a new hooks instance, so the grant is gone and it asks again.
    let mut resumed = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--resume",
            "s-session",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    resumed.write("go\n").await;
    resumed.wait_for_stderr("[y/N]").await;
    resumed.write("n\n").await;
    resumed.write("/exit\n").await;
    let (code, _, stderr) = resumed.finish().await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[y/N]"), "resume must ask again: {stderr}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: a compound runs without a prompt inside the
/// sandbox — the tier's blanket `allow` covers every segment (`approver = policy`), so
/// no allow rule is involved at all. The unsandboxed ask stays covered end to end by the
/// `outside_sandbox` tests (`a_shell_command_asks_every_time_when_allowed_once`,
/// `allow_for_session_covers_the_turn_and_not_a_resume`,
/// `never_turns_ask_into_a_denial`, `every_decision_is_audited_once_with_the_right_approver`);
/// the unit tests in `workspace_sandbox.rs` pin the unsandboxed compound ask.
#[tokio::test]
async fn a_sandboxed_compound_shell_command_runs_without_a_prompt() {
    let sessions = fresh_session_dir();
    let dir = scratch("compound-sandboxed");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // Inside the workspace: the shell really runs, and a confined sandbox still lets
    // workspace-local writes through, so the marker proves execution either way.
    let marker = ws.join("compound-ran.txt");
    let composite = format!("git status; touch {}", marker.display());
    let server = MockServer::start(vec![
        shell_reply("c1", "git status"),
        shell_reply("c2", &composite),
        text_reply("done"),
    ])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-compound",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    // No prompt to wait for: gate on the model's third request (two tool calls, then the
    // final text) instead.
    wait_until("both shell calls to reach the model", || {
        server.requests().len() >= 3
    })
    .await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask for a sandboxed shell: {stderr}"
    );
    assert!(marker.exists(), "the compound ran: {stderr}");
    let simple = tool_result_for(&server, 1, "c1");
    assert!(
        simple.contains("$ git status") && !simple.contains("denied"),
        "the simple command ran: {simple}"
    );
    let compound = tool_result_for(&server, 2, "c2");
    assert!(
        !compound.contains("denied"),
        "the compound was not denied: {compound}"
    );

    let raw = std::fs::read_to_string(sessions.join("s-compound.jsonl")).unwrap();
    let decisions: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["event"] == "PermissionResolved")
        .map(|value| value["data"].clone())
        .collect();
    for call in ["c1", "c2"] {
        let decision = decisions
            .iter()
            .find(|d| d["call_id"] == call)
            .unwrap_or_else(|| panic!("no decision for {call}: {decisions:#?}"));
        assert_eq!(decision["decision"], "allow", "{decision:#?}");
        assert_eq!(
            decision["approver"], "policy",
            "the tier blanket decided it: {decision:#?}"
        );
    }
}

/// 6: `--approval never` turns an `ask` into a denial: the command never runs. Uses an
/// `outside_sandbox` call — a sandboxed `workspace` shell no longer asks (slice 1 of
/// `sandbox-auto-allow-shell`).
#[tokio::test]
async fn never_turns_ask_into_a_denial() {
    let dir = scratch("never-deny");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![outside_reply("c1", "echo hi"), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--approval",
                "never",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("outside the sandbox"),
        "the denial must name what was refused: {result}"
    );
    assert!(
        !result.contains("exit code: 0"),
        "the command never runs: {result}"
    );
}

/// `full` runs shell with no terminal and no `--yes`: nothing asks, so the command runs
/// and only real output comes back (`42` cannot come from a denial: denials echo the
/// command, which never contains it). Reverting the tier's blanket shell `allow` denies
/// the call, so this test is that mutation's end-to-end catcher.
#[tokio::test]
async fn full_runs_shell_without_a_terminal_and_without_yes() {
    let dir = scratch("full-shell-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        shell_reply("c1", "echo $((40 + 2))"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "full",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask in full: {stderr}"
    );
    let result = tool_result_for(&server, 1, "c1");
    assert!(result.contains("42"), "the command ran: {result}");
    assert!(!result.contains("denied"), "{result}");
    assert!(!result.contains("not approved"), "{result}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: headless `radian exec --profile workspace`
/// without `--yes` runs a sandboxed shell with no prompt — the tier's blanket `allow`
/// answers it (`approver = policy`), the way `full` already did. Reverting the blanket
/// `allow` denies the call (no terminal to ask in), so this test is that mutation's
/// end-to-end catcher.
#[tokio::test]
async fn workspace_runs_a_sandboxed_shell_without_a_terminal_and_without_yes() {
    let dir = scratch("workspace-shell-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        shell_reply("c1", "echo $((40 + 2))"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("[y/N]"),
        "nothing may ask for a sandboxed shell: {stderr}"
    );
    let result = tool_result_for(&server, 1, "c1");
    assert!(result.contains("42"), "the command ran: {result}");
    assert!(!result.contains("denied"), "{result}");
    assert!(!result.contains("not approved"), "{result}");
}

/// Slice 1 of `sandbox-auto-allow-shell`: the same headless run with an
/// `outside_sandbox` request is denied with a clear result — `ask` with no approver is
/// denied as before, and nothing re-runs the command outside automatically. Letting it
/// skip `ask` runs the call, so this test is that mutation's end-to-end catcher.
#[tokio::test]
async fn workspace_outside_sandbox_is_denied_without_a_terminal() {
    let dir = scratch("workspace-outside-no-yes");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        outside_reply("c1", "echo SHOULD-NOT-RUN"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_for(&server, 1, "c1");
    assert!(
        result.contains("not approved") || result.contains("denied"),
        "the outside call must be denied with a clear result: {result}"
    );
    assert!(
        !result.contains("exit code: 0"),
        "the command must not have run: {result}"
    );
}

/// 7: Ctrl-C while waiting for a shell approval cancels the turn; the process does not hang.
/// Uses an `outside_sandbox` call so there is an approval to cancel (a sandboxed
/// `workspace` shell no longer asks since slice 1).
#[tokio::test]
async fn cancelling_at_the_shell_approval_does_not_hang() {
    let dir = scratch("shell-cancel");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![outside_reply("c1", "echo hi"), text_reply("done")]).await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr("[y/N]").await;
    sigint(&child.child);
    // The approval wait is interrupted and the turn converges; nothing hangs.
    child.wait_for_stderr("cancelled").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    // The turn never reached a second model request: the approval was interrupted, not
    // answered, and the command did not run.
    assert_eq!(server.requests().len(), 1, "{stderr}");
}

/// 8: every decision writes exactly one `PermissionResolved`, with the right approver.
#[tokio::test]
async fn every_decision_is_audited_once_with_the_right_approver() {
    let sessions = fresh_session_dir();
    let dir = scratch("audit");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(ws.join("f.txt"), "content").unwrap();
    let outside_file = outside.join("x.txt");

    let three_calls = Reply::sse(&[
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "r1", "type": "function",
                 "function": {"name": "read", "arguments": "{\"path\":\"f.txt\"}"}},
                {"index": 1, "id": "s1", "type": "function",
                 "function": {"name": "shell", "arguments": json!({
                     "command": "echo hi",
                     "outside_sandbox": true,
                     "outside_sandbox_reason": "the test leaves the sandbox on purpose"}).to_string()}},
                {"index": 2, "id": "w1", "type": "function",
                 "function": {"name": "write",
                              "arguments": format!("{{\"path\":{},\"content\":\"x\"}}", json!(outside_file.to_string_lossy()))}}
            ]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let server = MockServer::start(vec![three_calls, text_reply("done")]).await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--profile",
            "workspace",
            "--session-id",
            "s-audit",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    // `read` is allowed by a rule (no prompt); a sandboxed `shell` no longer asks either
    // (slice 1 of `sandbox-auto-allow-shell`), so the audited `ask` is the
    // `outside_sandbox` call; `write` outside the workspace has no rule and falls to ask.
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("y\n").await;
    child.wait_for_stderr_count("[y/N]", 2).await;
    child.write("n\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;
    assert_eq!(code, 0, "{stderr}");

    let raw = std::fs::read_to_string(sessions.join("s-audit.jsonl")).unwrap();
    let decisions: Vec<Value> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["event"] == "PermissionResolved")
        .map(|value| value["data"].clone())
        .collect();
    assert_eq!(decisions.len(), 3, "{decisions:#?}");
    let by_call = |id: &str| -> Value {
        decisions
            .iter()
            .find(|d| d["call_id"] == id)
            .unwrap_or_else(|| panic!("no decision for {id}: {decisions:#?}"))
            .clone()
    };
    assert_eq!(by_call("r1")["decision"], "allow");
    assert_eq!(by_call("r1")["approver"], "policy", "a rule decided it");
    assert_eq!(by_call("s1")["decision"], "allow");
    assert_eq!(by_call("s1")["approver"], "user", "the human answered");
    assert_eq!(by_call("w1")["decision"], "deny");
    assert_eq!(by_call("w1")["approver"], "user", "the human answered");
    assert_eq!(
        decisions.len(),
        decisions
            .iter()
            .map(|d| d["call_id"].clone())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "one decision per call"
    );
}

/// 9: under `workspace`, a shell command cannot write outside the workspace.
#[tokio::test]
async fn shell_cannot_write_outside_the_workspace() {
    let dir = scratch("shell-escape");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("escaped.txt");
    let command = format!("echo x > {}", target.display());
    let server = MockServer::start(vec![shell_reply("c1", &command), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    let result = tool_result_content(&server, 1);
    // The sandbox denies the write, but the shell's exit status for a failed redirection is
    // its own: bash uses 1, dash (Linux's `/bin/sh`) uses 2. Assert what the test means — the
    // command failed — not one shell's number.
    let exit = result
        .lines()
        .find_map(|line| line.strip_prefix("exit code: "))
        .expect("the shell result has an `exit code:` line");
    assert_ne!(
        exit, "0",
        "the sandboxed command must fail (a redirection denied): {result}"
    );
    assert!(
        result.contains("sandbox denied: filesystem-write"),
        "the failure must carry the sandbox marker: {result}"
    );
    assert!(
        !target.exists(),
        "the sandbox let a write escape the workspace"
    );
}

// ---------- context budget (ADR 0018): a computed input budget, never a stop ----------

/// A tool call reply that also reports provider usage for the request it answers.
fn used_tool_reply(id: &str, name: &str, arguments: &str, input: u64) -> Reply {
    Reply::sse(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": 1, "total_tokens": input + 1}}),
    ])
}

fn budget_workspace(label: &str) -> (ScratchDir, PathBuf) {
    let dir = scratch(label);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (dir, ws)
}

/// 1: without `--context-window` no budget exists; the startup says so, and the turn runs.
#[tokio::test]
async fn without_a_window_the_budget_is_off_and_says_so() {
    let (_dir, ws) = budget_workspace("budget-off");
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 3300),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(
        !stderr.contains("ADR"),
        "the budget line names no ADR: {stderr}"
    );
    // No budget is invented: `B` is unknown, not a guess (ADR 0018).
    assert!(stderr.contains("B ?"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
    // The turn ran to completion: both requests were sent.
    assert_eq!(server.requests().len(), 2, "{stderr}");
}

/// ADR 0015 (a zero window is unset, never a real zero): `[model] context_window = 0` leaves
/// the window unknown in `radian exec`, exactly like the key being absent, and the turn runs
/// with the guard off. A reader that refuses `0` fails startup here; a host that read `0` as a
/// real window would carry a zero budget instead of no budget.
#[tokio::test]
async fn a_zero_context_window_in_the_config_is_unset_and_the_guard_is_off() {
    let (dir, ws) = budget_workspace("budget-zero-config");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[model]\ncontext_window = 0\n",
    );
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
    assert!(
        !stderr.contains("positive integer"),
        "a zero window is unset, not a config error: {stderr}"
    );
    assert_eq!(server.requests().len(), 1, "{stderr}");
}

/// ADR 0015: the flag's zero is a **usage error**, not the config's spelling of "unset". This
/// run gives the catalog a cached window (`C 20000, I 18000`, i.e. `B 15000`), so a host that
/// read `--context-window 0` as "not given" would start the turn with the catalog's window:
/// the test fails on the exit code, on the missing refusal and on the request the mock never
/// should have seen.
#[tokio::test]
async fn a_zero_context_window_flag_is_a_usage_error() {
    let (dir, ws) = budget_workspace("budget-zero-flag");
    // The cache the run would read (`--offline-catalog`), in the shape models.dev serves.
    let home = dir.join("radian");
    let cache = home.join("cache/catalog/models.json");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(
        &cache,
        r#"{"acme": {"id": "acme", "models": {"m": {"id": "m",
            "limit": {"context": 20000, "input": 18000, "output": 4096}}}}}"#,
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--catalog-provider",
                "acme",
                "--offline-catalog",
                "--context-window",
                "0",
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("--context-window must be at least 1 token"),
        "{stderr}"
    );
    assert!(!stdout.contains("done"), "{stdout}");
    assert_eq!(
        server.requests().len(),
        0,
        "the turn must not run: {stderr}"
    );
}

/// 2: over the input budget `B` the request is **still sent** and the turn completes
/// (ADR 0018 replaced the stop). `B = C - R = 20000 - 16384 = 3616`, and the reported input
/// (9000) is far over it.
#[tokio::test]
async fn an_over_budget_request_is_still_sent_and_the_turn_completes() {
    let (_dir, ws) = budget_workspace("budget-over");
    // C = 20000, so R = 5000 and B = 15000 (ADR 0018); the reported input (20000)
    // is over it.
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 20000),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "20000",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("B 15000 from --context-window"), "{stderr}");
    assert_eq!(
        server.requests().len(),
        2,
        "an over-budget request must still be sent: {stderr}"
    );
}

/// 3: a context window that cannot hold the output reservation is a startup error, before the
/// session file or any request (ADR 0018). With adaptive `R` it needs the
/// request's own output to reach `C`: Anthropic sends `max_tokens = 16384` by default, so an
/// 8192-token window cannot hold the reservation.
#[tokio::test]
async fn a_context_window_at_or_below_the_reservation_is_rejected() {
    let (_dir, ws) = budget_workspace("budget-too-small");
    let server = anth::MockServer::start(vec![]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "claude-x",
                "--provider",
                "anthropic",
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "8192",
                "-p",
                "go",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("not larger than the output reservation"),
        "{stderr}"
    );
    assert!(server.requests().is_empty(), "{stderr}");
}

/// 4: an over-budget turn records the normal outcome: no `context_budget`, and no
/// `ContextBudgetReached` audit event is written (ADR 0018). The event type still exists for
/// reading old journals.
#[tokio::test]
async fn an_over_budget_turn_records_a_normal_outcome() {
    let sessions = fresh_session_dir();
    let (_dir, ws) = budget_workspace("budget-durable");
    // C = 20000 -> B = 15000; the reported input (20000) is over it.
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 20000),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "20000",
                "--session-id",
                "s-budget",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let raw = std::fs::read_to_string(sessions.join("s-budget.jsonl")).unwrap();
    assert!(
        !raw.contains("ContextBudgetReached"),
        "no stop event may be written:\n{raw}"
    );
    assert!(
        !raw.contains("\"context_budget\""),
        "the outcome must not be a budget stop:\n{raw}"
    );
    assert!(raw.contains("\"completed\""), "{raw}");
}

/// Like [`spawn_radian`], but uses a caller-owned session directory, so a test can assert
/// that a failed start left it empty (dogfooding D2).
fn spawn_radian_in(session_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    write_auth_json(&home, envs);
    Command::new(common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", session_dir)
        // Isolate the catalog cache, the `~/.radian` root and the network, like `spawn_radian`.
        .env("HOME", fresh_session_dir())
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
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

fn assert_session_dir_empty(dir: &Path) {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.is_empty(),
        "a failed start left files behind: {names:?}"
    );
}

/// Acceptance 1 (CLI): a `--workspace` that does not exist is a workspace error that names
/// the path, does not blame the sandbox, does not suggest `full-access`, and creates no
/// session file (dogfooding D1/D2).
#[tokio::test]
async fn a_missing_workspace_is_a_workspace_error_and_leaves_no_session() {
    let dir = scratch("missing-ws");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let missing = dir.join("no-such-workspace");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                missing.to_str().unwrap(),
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "missing-ws",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains(missing.to_str().unwrap()), "{stderr}");
    assert!(
        !stderr.contains("sandbox backend"),
        "a bad workspace must not blame the sandbox: {stderr}"
    );
    assert!(
        !stderr.contains("--profile"),
        "a bad workspace must not suggest a profile (old or new name): {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert_session_dir_empty(&session_dir);
}

/// ADR 0020: a `--session-dir` that is an ancestor of the workspace refuses at startup with
/// the same exit code as the other sandbox refusals, names the problem (not the backend), and
/// leaves no session file.
#[tokio::test]
async fn a_session_dir_above_the_workspace_is_refused_and_leaves_no_session() {
    let dir = scratch("nested-ws");
    let session_dir = dir.join("sessions");
    let ws = session_dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "nested-ws",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("workspace is inside"), "{stderr}");
    assert!(
        !stderr.contains("sandbox backend"),
        "a nested workspace must not blame the backend: {stderr}"
    );
    assert!(
        !stderr.contains("--profile"),
        "a nested workspace must not suggest a profile: {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    // The workspace lives inside the session dir, so the dir is not empty; the point is that
    // no session file was created.
    assert!(
        !session_dir.join("nested-ws.jsonl").exists(),
        "a failed start left a session file behind"
    );
}

/// Acceptance 2 and 4 (CLI): a failed sandbox probe refuses, leaves no session file, and the
/// same `--session-id` with corrected parameters then creates the session (dogfooding D2).
#[tokio::test]
async fn a_failed_probe_leaves_no_session_and_the_retry_creates_one() {
    let dir = scratch("probe-no-session");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;
    let base_url = server.base_url.clone();
    let ws_str = ws.to_str().unwrap().to_owned();
    let sessions = session_dir.to_str().unwrap().to_owned();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                &base_url,
                "--workspace",
                &ws_str,
                "--profile",
                "workspace",
                "--yes",
                "-p",
                "hi",
                "--session-dir",
                &sessions,
                "--session-id",
                "retry",
            ],
            &[("RADIAN_SANDBOX_PROBE_FAIL", "1")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("sandbox backend is unavailable") || stderr.contains("cannot be enforced"),
        "{stderr}"
    );
    assert_session_dir_empty(&session_dir);

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                &base_url,
                "--workspace",
                &ws_str,
                "--yes",
                "-p",
                "hi",
                "--session-dir",
                &sessions,
                "--session-id",
                "retry",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        session_dir.join("retry.jsonl").exists(),
        "the corrected retry created the session"
    );
}

/// Acceptance 3 (CLI): a malformed `--mcp-config` fails before the session file exists.
#[tokio::test]
async fn a_bad_mcp_config_leaves_no_session() {
    let dir = scratch("bad-mcp");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let bad = write_file(&dir, "bad.json", "{\"servers\": [], \"bogus\": 1}");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--workspace",
                ws.to_str().unwrap(),
                "--mcp-config",
                bad.to_str().unwrap(),
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "bad-mcp",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains(bad.to_str().unwrap()), "{stderr}");
    assert_session_dir_empty(&session_dir);
}

// ---------- user [mcp] (ADR 0008 §8) ----------

/// Writes a user `config.toml` whose `[mcp]` points one `fx` server at the fixture,
/// logging to `log`. `form` is `"servers"` (`[[mcp.servers]]`) or `"mcpServers"`
/// (`[mcp.mcpServers.fx]`).
fn mcp_user_config(dir: &Path, form: &str, log: &Path) -> PathBuf {
    let fixture = fixture_bin();
    let body = match form {
        "servers" => format!(
            "schema_version = 1\n[[mcp.servers]]\nid = \"fx\"\ncommand = \"{}\"\n\
             [mcp.servers.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            log.display()
        ),
        _ => format!(
            "schema_version = 1\n[mcp.mcpServers.fx]\ncommand = \"{}\"\n\
             [mcp.mcpServers.fx.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            log.display()
        ),
    };
    write_file(dir, "user-config.toml", &body)
}

/// `[mcp]` with `[[mcp.servers]]` reaches the session: the fixture echo runs with no
/// `--mcp-config` on the command line.
#[tokio::test]
async fn mcp_servers_from_the_user_config_run() {
    let dir = scratch("mcp-user-servers");
    let log = dir.join("fixture.log");
    let config = mcp_user_config(&dir, "servers", &log);
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

/// `[mcp]` with `[mcp.mcpServers.fx]` reaches the session the same way.
#[tokio::test]
async fn mcp_mcp_servers_from_the_user_config_run() {
    let dir = scratch("mcp-user-shared");
    let log = dir.join("fixture.log");
    let config = mcp_user_config(&dir, "mcpServers", &log);
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
}

/// On an id collision the `--mcp-config` file wins: only its server process ever starts
/// (the config's `FIXTURE_LOG` stays empty), and one `[mcp]` warning names the id.
#[tokio::test]
async fn an_mcp_id_collision_uses_the_command_line_with_one_warning() {
    let dir = scratch("mcp-collision");
    let config_log = dir.join("config-fixture.log");
    let config_path = mcp_user_config(&dir, "servers", &config_log);
    let file_log = dir.join("file-fixture.log");
    let mut file_env = serde_json::Map::new();
    file_env.insert(
        "FIXTURE_LOG".to_owned(),
        serde_json::Value::String(file_log.to_string_lossy().into_owned()),
    );
    let file = write_file(
        &dir,
        "mcp.json",
        &json!({"servers": [{
            "id": "fx",
            "command": fixture_bin(),
            "env": file_env,
        }]})
        .to_string(),
    );
    let server = MockServer::start(echo_then("done")).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config_path.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                file.to_str().unwrap(),
                "-p",
                "go",
                "--yes",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
    // The file's server ran; the config's never started.
    assert!(
        !log_lines(&file_log).is_empty(),
        "the winning server must log its methods"
    );
    assert!(
        std::fs::read_to_string(&config_log)
            .unwrap_or_default()
            .is_empty(),
        "the config's colliding server must never start"
    );
    let collisions: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("[mcp]") && line.contains("--mcp-config"))
        .collect();
    assert_eq!(collisions.len(), 1, "{stderr}");
    assert!(collisions[0].contains("`fx`"), "{stderr}");
}

/// Both `[mcp]` forms at once is a config error: startup refuses before any session file
/// exists, like a malformed `--mcp-config`.
#[tokio::test]
async fn both_mcp_config_forms_are_refused_before_any_session() {
    let dir = scratch("mcp-both-forms");
    let session_dir = dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();
    let config = write_file(
        &dir,
        "user-config.toml",
        "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n[mcp.mcpServers]\n",
    );
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &session_dir,
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:9/v1",
                "--session-dir",
                session_dir.to_str().unwrap(),
                "--session-id",
                "mcp-both-forms",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot be combined"), "{stderr}");
    assert_session_dir_empty(&session_dir);
}

/// `--no-config-mcp` (the `explore` subagent child's flag) ignores the config file's
/// `[mcp]`: the fixture server never starts, so the model is told there is no such
/// tool. The same config without the flag starts the server (the control, so the test
/// cannot pass for the wrong reason: a broken fixture path would fail the control, not
/// silently pass the flag case).
#[tokio::test]
async fn no_config_mcp_ignores_the_config_mcp_section() {
    for (name, no_config_mcp) in [("flag", true), ("control", false)] {
        let dir = scratch(&format!("no-config-mcp-{name}"));
        let log = dir.join("fixture.log");
        let config = mcp_user_config(&dir, "servers", &log);
        let server = MockServer::start(echo_then("done")).await;
        let mut args = vec![
            "--config",
            config.to_str().unwrap(),
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--yes",
        ];
        if no_config_mcp {
            args.push("--no-config-mcp");
        }

        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;

        assert_eq!(code, 0, "{name}: {stderr}");
        let started = !std::fs::read_to_string(&log).unwrap_or_default().is_empty();
        if no_config_mcp {
            assert!(!started, "{name}: the config server must never start");
            assert_ne!(
                tool_result_content(&server, 1),
                r#"{"hello":"mcp"}"#,
                "{name}: no MCP tool may be offered"
            );
        } else {
            assert!(started, "{name}: the control must start the config server");
            assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        }
    }
}

// ---------- project .mcp.json (slice B2) ----------

/// Writes `<ws>/.mcp.json` pointing one `fx` server at the fixture, in the shared
/// `mcpServers` form, with `extra_env` merged into the server's `env`.
fn mcp_project_config(ws: &Path, log: &Path, extra_env: &[(&str, &str)]) -> PathBuf {
    let mut env = serde_json::Map::new();
    env.insert(
        "FIXTURE_LOG".to_owned(),
        json!(log.to_string_lossy().into_owned()),
    );
    for (key, value) in extra_env {
        env.insert((*key).to_owned(), json!(value));
    }
    write_file(
        ws,
        ".mcp.json",
        &json!({"mcpServers": {"fx": {"command": fixture_bin(), "env": env}}}).to_string(),
    )
}

/// A minimal user config with no `[mcp]` section.
fn minimal_user_config(dir: &Path) -> PathBuf {
    write_file(dir, "user-config.toml", "schema_version = 1\n")
}

/// B2: an untrusted workspace starts no project server process (the fixture log stays
/// empty) and the startup line says the file was not loaded; `--trust` loads it (the
/// control, so the test cannot pass for the wrong reason: a broken fixture path would
/// fail the trusted case, not silently pass the untrusted one). The mutation (untrusted
/// loads too) fails the untrusted assertions.
#[tokio::test]
async fn project_mcp_loads_only_when_trusted() {
    for (name, trust) in [("untrusted", false), ("trusted", true)] {
        let dir = scratch(&format!("project-mcp-{name}"));
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let log = dir.join("fixture.log");
        mcp_project_config(&ws, &log, &[]);
        let config = minimal_user_config(&dir);
        let server = MockServer::start(echo_then("done")).await;
        let mut args = vec![
            "--config",
            config.to_str().unwrap(),
            "--workspace",
            ws.to_str().unwrap(),
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "go",
            "--yes",
        ];
        if trust {
            args.push("--trust");
        }
        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 0, "{name}: {stderr}");
        let started = !std::fs::read_to_string(&log).unwrap_or_default().is_empty();
        if trust {
            assert!(
                stderr.contains("project .mcp.json: loaded 1 server"),
                "{name}: {stderr}"
            );
            assert!(started, "{name}: the project server must start");
            assert_eq!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        } else {
            assert!(!started, "{name}: untrusted must start no project server");
            assert!(
                stderr.contains("project .mcp.json not loaded (untrusted)"),
                "{name}: {stderr}"
            );
            assert_ne!(tool_result_content(&server, 1), r#"{"hello":"mcp"}"#);
        }
    }
}

/// B2: a trusted project that edits its `.mcp.json` gets the new content on restart with
/// no new question: run 1 loads the fixture server, run 2 (only a remote entry left)
/// loads nothing and warns skipped. The trust decision is seeded in a private
/// `RADIAN_HOME`, like a previous `/trust` choice.
#[tokio::test]
async fn a_trusted_project_reloads_an_edited_mcp_json_without_reasking() {
    let dir = scratch("project-mcp-restart");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let canonical = ws.canonicalize().unwrap();
    let home = dir.join("home");
    std::fs::create_dir_all(home.join("trust")).unwrap();
    std::fs::write(
        home.join("trust/trust.json"),
        format!(r#"{{"{}": true}}"#, canonical.to_string_lossy()),
    )
    .unwrap();
    let config = minimal_user_config(&dir);
    let home_str = home.to_str().unwrap().to_owned();

    // Run 1: one stdio server at the fixture.
    let log = dir.join("fixture.log");
    mcp_project_config(&ws, &log, &[]);
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("RADIAN_HOME", home_str.as_str())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("project .mcp.json: loaded 1 server"),
        "run 1: {stderr}"
    );
    assert!(
        !std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "run 1: the project server must start"
    );
    assert!(
        !stderr.contains("trust them?"),
        "run 1: must not re-ask: {stderr}"
    );
    drop(server);

    // Run 2: the project file now holds only a remote entry.
    write_file(
        &ws,
        ".mcp.json",
        r#"{"mcpServers":{"web": {"type": "http", "url": "https://example.com/mcp"}}}"#,
    );
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("RADIAN_HOME", home_str.as_str())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("project .mcp.json: loaded 0 servers"),
        "run 2 reads the edited file: {stderr}"
    );
    assert!(stderr.contains("skipped `web`"), "run 2: {stderr}");
    assert!(
        !stderr.contains("trust them?"),
        "run 2: must not re-ask: {stderr}"
    );
}

/// B2: a credential variable in a project entry expands to empty end to end (the fixture
/// `env` tool reads `TOKEN` back), while the same entry as a user `[mcp]` section expands
/// as today. The mutation (project entries expand credentials) fails the first assertion.
#[tokio::test]
async fn project_credential_variables_expand_to_empty_end_to_end() {
    let replies = || {
        vec![
            tool_reply("call_1", "fx__env", r#"{"name":"PROBE"}"#),
            text_reply("done"),
        ]
    };
    // The project entry: blanked.
    let dir = scratch("project-mcp-credential");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.join("fixture.log");
    // `PROBE` (not a `*_TOKEN`-style sensitive name): the user config below must parse —
    // a bare string on a sensitive key is refused as a plaintext secret (ADR 0008 §3).
    // The blanking keys on the *variable*, not the key, so the probe still proves it.
    mcp_project_config(&ws, &log, &[("PROBE", "${ANTHROPIC_API_KEY}")]);
    let config = minimal_user_config(&dir);
    let server = MockServer::start(replies()).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
            ],
            &[("ANTHROPIC_API_KEY", "sk-b2-live-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        tool_result_content(&server, 1),
        "",
        "the project server must not see the shell credential: {stderr}"
    );
    drop(server);
    // The same entry as a user `[mcp]` section: expanded as today.
    let dir = scratch("user-mcp-credential");
    let log = dir.join("fixture.log");
    let fixture = fixture_bin();
    let config = write_file(
        &dir,
        "user-config.toml",
        &format!(
            "schema_version = 1\n[mcp.mcpServers.fx]\ncommand = \"{}\"\n\
             [mcp.mcpServers.fx.env]\nFIXTURE_LOG = \"{}\"\nPROBE = \"${{ANTHROPIC_API_KEY}}\"\n",
            fixture.display(),
            log.display(),
        ),
    );
    let server = MockServer::start(replies()).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
            ],
            &[("ANTHROPIC_API_KEY", "sk-b2-live-test")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        tool_result_content(&server, 1),
        "sk-b2-live-test",
        "user entries keep today's expansion: {stderr}"
    );
}

/// B2: `--no-config-mcp` (the `explore` child's flag) skips the project file even when
/// trusted: no process starts, one line says so.
#[tokio::test]
async fn no_config_mcp_skips_the_project_file() {
    let dir = scratch("project-mcp-no-config");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let log = dir.join("fixture.log");
    mcp_project_config(&ws, &log, &[]);
    let config = minimal_user_config(&dir);
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
                "--no-config-mcp",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        std::fs::read_to_string(&log).unwrap_or_default().is_empty(),
        "the project server must never start"
    );
    assert!(
        stderr.contains("project .mcp.json skipped (--no-config-mcp)"),
        "{stderr}"
    );
}

/// B2: the project file loses to both user sources: `--mcp-config` wins the `fx`
/// collision and `[mcp]` wins the `cfg` collision, each with one warning naming the id
/// and the winning side; only the winning servers' processes ever start.
#[tokio::test]
async fn a_project_collision_uses_the_command_line_with_one_warning() {
    let dir = scratch("project-mcp-collision");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // The project file holds both colliding ids.
    let project_fx_log = dir.join("project-fx.log");
    let project_cfg_log = dir.join("project-cfg.log");
    let fixture = fixture_bin();
    write_file(
        &ws,
        ".mcp.json",
        &json!({"mcpServers": {
            "fx": {"command": fixture,
                    "env": {"FIXTURE_LOG": project_fx_log.to_string_lossy().into_owned()}},
            "cfg": {"command": fixture,
                    "env": {"FIXTURE_LOG": project_cfg_log.to_string_lossy().into_owned()}},
        }})
        .to_string(),
    );
    // `[mcp]` holds `cfg`; `--mcp-config` holds `fx`.
    let cfg_log = dir.join("cfg.log");
    let config = write_file(
        &dir,
        "user-config.toml",
        &format!(
            "schema_version = 1\n[mcp.mcpServers.cfg]\ncommand = \"{}\"\n\
             [mcp.mcpServers.cfg.env]\nFIXTURE_LOG = \"{}\"\n",
            fixture.display(),
            cfg_log.display(),
        ),
    );
    let file_fx_log = dir.join("file-fx.log");
    let file = write_file(
        &dir,
        "mcp.json",
        &json!({"servers": [{
            "id": "fx",
            "command": fixture_bin(),
            "env": {"FIXTURE_LOG": file_fx_log.to_string_lossy().into_owned()},
        }]})
        .to_string(),
    );
    let server = MockServer::start(echo_then("done")).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--mcp-config",
                file.to_str().unwrap(),
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "go",
                "--yes",
                "--trust",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    for (log, name) in [
        (&project_fx_log, "project fx"),
        (&project_cfg_log, "project cfg"),
    ] {
        assert!(
            std::fs::read_to_string(log).unwrap_or_default().is_empty(),
            "the {name} server must never start"
        );
    }
    for (log, name) in [(&file_fx_log, "command-line fx"), (&cfg_log, "config cfg")] {
        assert!(
            !std::fs::read_to_string(log).unwrap_or_default().is_empty(),
            "the {name} server must start"
        );
    }
    assert!(
        stderr
            .contains("server `fx` is set in both .mcp.json and --mcp-config; using --mcp-config"),
        "{stderr}"
    );
    assert!(
        stderr.contains("server `cfg` is set in both .mcp.json and [mcp]; using [mcp]"),
        "{stderr}"
    );
}

// ---------- provider selection: anthropic (Phase 7 / anthost) ----------

/// One Anthropic text reply: the minimal end_turn event stream (wire notes §4).
fn anth_text_reply(text: &str) -> anth::Reply {
    anth::Reply::sse(&[
        anth::message_start(Some(json!({"input_tokens": 3, "output_tokens": 1}))),
        anth::text_start(0),
        anth::text_delta(0, text),
        anth::block_stop(0),
        anth::message_delta("end_turn", Some(json!({"output_tokens": 2}))),
        anth::message_stop(),
    ])
}

/// `--provider anthropic` talks the Messages API: `/v1/messages`, `x-api-key` and
/// `anthropic-version`, and the reply is shown.
#[tokio::test]
async fn anthropic_provider_sends_a_messages_request_and_shows_the_reply() {
    let server = anth::MockServer::start(vec![anth_text_reply("ANTHROPIC-REPLY-OK")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "claude-x", "--provider", "anthropic", "-p", "hi"],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ANTHROPIC-REPLY-OK"), "{stdout}");
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "one request expected");
    assert_eq!(requests[0].path, "/v1/messages");
    assert_eq!(requests[0].header("x-api-key"), Some("sk-ant-test-key"));
    assert_eq!(requests[0].header("anthropic-version"), Some("2023-06-01"));
    let body = requests[0].json();
    assert_eq!(body["model"], "claude-x");
    // `max_tokens` is required by the Messages API and the host fills it in.
    assert_eq!(body["max_tokens"], 16384, "{body}");
    // ADR 0026 D11: the default level is `medium`, and the anthropic descriptor's provider
    // map sends it as the existing `adaptive` setting.
    assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{body}");
}

/// Without `--api-key-env`, `--provider anthropic` reads `ANTHROPIC_API_KEY`; the key never
/// appears in the output or the session file.
#[tokio::test]
async fn anthropic_reads_anthropic_api_key_env_by_default_and_never_leaks_it() {
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--session-id",
                "anthro-env",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-default-env-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        server.requests()[0].header("x-api-key"),
        Some("sk-ant-default-env-key")
    );
    for text in [&stdout, &stderr] {
        assert!(
            !text.contains("sk-ant-default-env-key"),
            "the key leaked into the output: {text}"
        );
    }
    let session = std::fs::read_to_string(dir.join("anthro-env.jsonl")).unwrap();
    assert!(
        !session.contains("sk-ant-default-env-key"),
        "the key reached the session file"
    );
}

/// ADR 0026 D11 (review A1): every level has its own wire value on Anthropic — `adaptive` for
/// the default `medium`, `enabled:<budget>` for the rest — and a legacy
/// `--thinking enabled:<budget>` keeps the user's budget. `off` sends no `thinking` key,
/// asserted in the messages test below.
#[tokio::test]
async fn anthropic_thinking_modes_are_sent_in_the_body() {
    for (mode, expected) in [
        ("adaptive", json!({"type": "adaptive"})),
        ("high", json!({"type": "enabled", "budget_tokens": 4096})),
        ("max", json!({"type": "enabled", "budget_tokens": 12288})),
        (
            "enabled:8192",
            json!({"type": "enabled", "budget_tokens": 8192}),
        ),
    ] {
        let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "claude",
                    "--provider",
                    "anthropic",
                    "--thinking",
                    mode,
                    "-p",
                    "hi",
                ],
                &[
                    ("ANTHROPIC_API_KEY", "sk-ant-thinking"),
                    ("RADIAN_BASE_URL", &server.base_url),
                ],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{mode}: {stderr}");
        assert_eq!(server.requests()[0].json()["thinking"], expected, "{mode}");
    }
}

/// The session config snapshot names the real provider; the default provider is unchanged.
#[tokio::test]
async fn the_session_snapshot_names_the_real_provider() {
    // anthropic
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--session-id",
                "snap-ant",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-snap"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let session = std::fs::read_to_string(dir.join("snap-ant.jsonl")).unwrap();
    assert!(session.contains("\"provider\":\"anthropic\""), "{session}");

    // openai-compatible (the default, no --provider flag)
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "snap-oai",
                "-p",
                "hi",
            ],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let session = std::fs::read_to_string(dir.join("snap-oai.jsonl")).unwrap();
    assert!(
        session.contains("\"provider\":\"openai-compatible\""),
        "{session}"
    );
}

/// The default provider still sends an OpenAI-compatible request: `/v1/chat/completions`
/// (the adapter's path) and a bearer token.
#[tokio::test]
async fn the_default_provider_still_sends_an_openai_request() {
    let server = MockServer::start(vec![text_reply("OPENAI-REPLY")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("OPENAI-REPLY"), "{stdout}");
    let request = &server.requests()[0];
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {KEY}").as_str())
    );
}

/// `--thinking enabled:<budget>` is checked before the session file exists: the budget must be
/// `>= 1024` and `< --max-tokens` (Anthropic's rule, wire notes §2.3). Both a budget equal to
/// `max_tokens` (16384 default) and one below 1024 are usage errors and leave no session file.
#[tokio::test]
async fn the_legacy_enabled_budget_is_accepted_and_clamped_like_high() {
    // ADR 0026 D11 (review A1): the legacy `enabled:<budget>` is accepted for one version with
    // a warning and the user's budget is sent **as-is** (not replaced by the level's budget).
    let server = anth::MockServer::start(vec![anth_text_reply("LEGACY-OK")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--thinking",
                "enabled:8192",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-budget"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("LEGACY-OK"), "{stdout}");
    assert!(
        stderr.contains("deprecated"),
        "the legacy spelling warns: {stderr}"
    );
    let body = server.requests()[0].json();
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 8192}),
        "{body}"
    );
}

/// Review B1: a **level's** budget is clamped to `--max-tokens - 1` instead of failing the
/// build, and the completion hides the levels only when no budget can fit at all. The user's
/// explicit legacy budget is not clamped.
#[tokio::test]
async fn a_level_budget_clamps_to_max_tokens() {
    // `--max-tokens 4096` + `--thinking max` (12288 in the map) -> 4095 on the wire.
    let server = anth::MockServer::start(vec![anth_text_reply("CLAMPED")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "4096",
                "--thinking",
                "max",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-clamp"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("CLAMPED"), "{stdout}");
    let body = server.requests()[0].json();
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 4095}),
        "{body}"
    );

    // `--max-tokens 1024` leaves no room for any budget: an `enabled` level clamps to a
    // budget-less one, so the request has no `enabled` setting.
    let server = anth::MockServer::start(vec![anth_text_reply("NO-ROOM")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "1024",
                "--thinking",
                "high",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-floor"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("NO-ROOM"), "{stdout}");
    let body = server.requests()[0].json();
    assert!(
        !body["thinking"].to_string().contains("enabled"),
        "no enabled budget fits 1024: {body}"
    );

    // The user's explicit legacy budget is **not** clamped: over-limit stays a usage error.
    let dir = fresh_session_dir();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--max-tokens",
                "4096",
                "--thinking",
                "enabled:99999",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-legacy")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("out of range"), "{stderr}");
}

// ---------- session export and search (--export / --search) ----------

/// Writes a minimal, valid v1 session file by hand: one user message and one assistant
/// message, optionally carrying reasoning and a raw provider payload. Used where a real run
/// cannot produce the field (the OpenAI adapter never sets a provider payload).
fn write_session(
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

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The sorted file names in a directory, for the "writes nothing" assertion.
fn dir_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The `.jsonl` session files in a directory, name and bytes. `index.db` and its WAL files
/// are a disposable cache (ADR 0006 §10), not session files, so they are excluded.
fn jsonl_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
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

/// Acceptance 1: `--export` renders the conversation as Markdown by default and as JSON with
/// `--format json`.
#[tokio::test]
async fn export_renders_markdown_and_json() {
    let dir = scratch("export-basic");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("REPLY-TEXT-ONE")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "e1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "USER-TEXT-ONE",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "e1",
    ];
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("# Session e1"), "{stdout}");
    assert!(stdout.contains("USER-TEXT-ONE"), "{stdout}");
    assert!(stdout.contains("REPLY-TEXT-ONE"), "{stdout}");

    let mut json_args = base.to_vec();
    json_args.extend_from_slice(&["--format", "json"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&json_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let parsed: Value = serde_json::from_str(&stdout).expect("the JSON export is valid JSON");
    assert_eq!(parsed["session_id"], "e1");
    assert!(stdout.contains("USER-TEXT-ONE"), "{stdout}");
    assert!(stdout.contains("REPLY-TEXT-ONE"), "{stdout}");
}

/// Acceptance 2: reasoning and provider payload are omitted by default and included only
/// with their switches.
#[tokio::test]
async fn export_omits_reasoning_and_provider_payload_by_default() {
    let dir = scratch("export-optin");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "optin",
        &canonical(&ws),
        "VISIBLE-ANSWER",
        Some("SECRET-REASONING"),
        Some(r#"{"marker":"PAYLOAD-MARKER"}"#),
    );
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "optin",
    ];

    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("VISIBLE-ANSWER"), "{stdout}");
    assert!(!stdout.contains("SECRET-REASONING"), "{stdout}");
    assert!(!stdout.contains("PAYLOAD-MARKER"), "{stdout}");

    let mut with_reasoning = base.to_vec();
    with_reasoning.push("--include-reasoning");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&with_reasoning, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SECRET-REASONING"), "{stdout}");
    assert!(!stdout.contains("PAYLOAD-MARKER"), "{stdout}");

    let mut with_payload = base.to_vec();
    with_payload.push("--include-provider-payload");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&with_payload, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("PAYLOAD-MARKER"), "{stdout}");
    assert!(!stdout.contains("SECRET-REASONING"), "{stdout}");
}

/// Acceptance 3: the process's key and every `--header` value are redacted from the export.
#[tokio::test]
async fn export_redacts_the_key_and_header_values() {
    let dir = scratch("export-secrets");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "sec",
        &canonical(&ws),
        "the key sk-cli-test-secret and the header hdr-value-xyz",
        None,
        None,
    );
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--export",
                "sec",
                "--header",
                "X-Test=hdr-value-xyz",
            ],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains(KEY), "the key leaked: {stdout}");
    assert!(
        !stdout.contains("hdr-value-xyz"),
        "the header leaked: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

/// Acceptance 4: a session that belongs to another workspace is refused by name.
#[tokio::test]
async fn export_refuses_another_workspaces_session() {
    let dir = scratch("export-other-ws");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws_a = dir.join("ws-a");
    let ws_b = dir.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    write_session(&sessions, "in-b", &canonical(&ws_b), "text", None, None);
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws_a.to_str().unwrap(),
                "--export",
                "in-b",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("not in this workspace"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
}

/// ADR 0006 §4: `--resume` applies the same workspace rule as `--export` and ACP's
/// `session/load`. A session created in another workspace is refused by name, and the journal
/// is not written.
#[tokio::test]
async fn resume_refuses_another_workspaces_session() {
    // `common::process_dir`, not `scratch`: the sandbox cannot create `/tmp` directories, and
    // this test opens no Unix socket (the reason `scratch` uses `/tmp`).
    let dir = common::process_dir("radian-exec-resume-other-ws");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws_a = dir.join("ws-a");
    let ws_b = dir.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let session = write_session(&sessions, "from-a", &canonical(&ws_a), "reply", None, None);
    let before = std::fs::read(&session).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "http://127.0.0.1:1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws_b.to_str().unwrap(),
                "--resume",
                "from-a",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("from-a") && stderr.contains("not in this workspace"),
        "the error names the session and the rule: {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(
        std::fs::read(&session).unwrap(),
        before,
        "a refused resume writes nothing to the journal"
    );
}

/// Acceptance 5: `--out` writes a file, refuses an existing one, and `--force` overwrites it.
#[tokio::test]
async fn export_out_refuses_to_overwrite_without_force() {
    let dir = scratch("export-out");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(&sessions, "out", &canonical(&ws), "OUT-TEXT", None, None);
    let out = dir.join("export.md");
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
        "--export",
        "out",
        "--out",
        out.to_str().unwrap(),
    ];

    let (code, _, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let first = std::fs::read_to_string(&out).unwrap();
    assert!(first.contains("OUT-TEXT"), "{first}");

    let (code, _, stderr) = feed_and_wait(spawn_radian(&base, &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), first);

    std::fs::write(&out, "OLD").unwrap();
    let mut forced = base.to_vec();
    forced.push("--force");
    let (code, _, stderr) = feed_and_wait(spawn_radian(&forced, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert_ne!(std::fs::read_to_string(&out).unwrap(), "OLD");
}

/// Acceptance 6: `--search` prints one line per hit and reports "no match" on stderr with a
/// zero exit when nothing matches.
#[tokio::test]
async fn search_finds_text_and_reports_no_match() {
    let dir = scratch("search");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("SEARCHABLE-MARKER here")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "s1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "find me",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    let mut hit_args = base.to_vec();
    hit_args.extend_from_slice(&["--search", "SEARCHABLE-MARKER"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&hit_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    let row = stdout.lines().next().unwrap();
    assert!(row.starts_with("s1\t"), "{row}");
    assert_eq!(row.split('\t').count(), 3, "{row}");
    assert!(row.contains("SEARCHABLE-MARKER"), "{row}");

    let mut miss_args = base.to_vec();
    miss_args.extend_from_slice(&["--search", "NO-SUCH-TEXT-XYZ"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&miss_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("no match"), "{stderr}");
}

/// Acceptance 7: the session commands write nothing into the session directory (not even an
/// index cache; `--search` builds one in the system temp directory).
#[tokio::test]
async fn session_commands_do_not_touch_the_session_directory() {
    let dir = scratch("session-readonly");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("KEEP-MARKER")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "keep",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "keep me",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let before = jsonl_snapshot(&sessions);

    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    let mut export_args = base.to_vec();
    export_args.extend_from_slice(&["--export", "keep"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&export_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let mut search_args = base.to_vec();
    search_args.extend_from_slice(&["--search", "KEEP-MARKER"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&search_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");

    assert_eq!(
        jsonl_snapshot(&sessions),
        before,
        "a session command created or modified a .jsonl file"
    );
    // The search cache (`index.db`) may be created or updated: it is disposable, not a
    // session (ADR 0006 §10).
    assert!(
        dir_names(&sessions).iter().any(|name| name == "index.db"),
        "this test no longer exercises the shared index cache"
    );
}

/// The session commands are their own mode: they cannot be combined with a prompt or a
/// conversation, which is a clap usage error (exit 2).
#[tokio::test]
async fn session_commands_conflict_with_a_prompt() {
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["--model", "m", "--export", "x", "-p", "hi"], &[]),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

/// `--model` is not required for the session-management commands: none of them starts a
/// conversation or reads the key.
#[tokio::test]
async fn session_commands_do_not_need_a_model() {
    let dir = scratch("no-model");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "nm",
        &canonical(&ws),
        "NO-MODEL-TEXT",
        None,
        None,
    );
    let base = [
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];

    let mut list_args = base.to_vec();
    list_args.push("--list");
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&list_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("nm"), "{stdout}");

    let mut export_args = base.to_vec();
    export_args.extend_from_slice(&["--export", "nm"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&export_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("NO-MODEL-TEXT"), "{stdout}");

    let mut search_args = base.to_vec();
    search_args.extend_from_slice(&["--search", "NO-MODEL-TEXT"]);
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&search_args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("nm"), "{stdout}");
}

/// A conversation still requires a model: with no `--model` and no `model.model` in the
/// config file, `-p` and interactive mode are usage errors (exit 2, before any session is
/// created). The decision moved from clap to `setup` so the config file can supply it.
/// ADR 0008 §2: the message names the config file path and the `--provider <p> --model <m>`
/// form.
#[tokio::test]
async fn a_conversation_still_requires_a_model() {
    // The setup check runs **before** the API-key lookup, and its wording is unique: asserting
    // only `--model` would also be satisfied by the later "no API key" error, so a mutation
    // that dropped the model requirement would slip past. Assert the sentence itself.
    let (code, _, stderr) = feed_and_wait(spawn_radian(&["-p", "hi"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("no model: pass --provider <p> --model <m>"),
        "{stderr}"
    );

    let (code, _, stderr) = feed_and_wait(spawn_radian(&[], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("no model: pass --provider <p> --model <m>"),
        "{stderr}"
    );
}

/// ADR 0008 §2: a fresh `RADIAN_HOME` gets a commented default config on startup; the
/// no-model failure names its path. A second run leaves it untouched (bytes and mode), and
/// an explicit `--config` path that does not exist is never created.
#[tokio::test]
async fn first_launch_creates_a_default_config_and_names_it() {
    let dir = scratch("first-launch");
    let home = dir.0.join("home");
    let expected = home.join("config/config.toml");
    // First run: the file is created, the message names it and the `--provider/--model` form.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["-p", "hi"], &[("RADIAN_HOME", home.to_str().unwrap())]),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains(&format!(
            "created a default config at {}",
            expected.display()
        )),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "no model: pass --provider <p> --model <m>, or set model.provider/model in {}",
            expected.display()
        )),
        "{stderr}"
    );
    assert!(expected.is_file(), "the default config was created");
    let before = std::fs::read(&expected).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&expected).unwrap().permissions().mode() & 0o777,
            0o600,
            "the fresh config is owner-only"
        );
        assert_eq!(
            std::fs::metadata(expected.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "the config directory is owner-only"
        );
    }
    // The template parses to the default config (every key commented out).
    let parsed = radian_host::parse_config(&expected, &std::fs::read_to_string(&expected).unwrap())
        .expect("the template must parse");
    assert_eq!(parsed.model, radian_host::ModelConfig::default());
    // Second run: the file is not touched.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&["-p", "hi"], &[("RADIAN_HOME", home.to_str().unwrap())]),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        !stderr.contains("created a default config"),
        "an existing file is never rewritten: {stderr}"
    );
    assert_eq!(
        std::fs::read(&expected).unwrap(),
        before,
        "not one byte changes"
    );
    // An explicit missing path is never created (a typo must not leave a file).
    let missing = dir.0.join("custom.toml");
    let (code, _, _) = feed_and_wait(
        spawn_radian(
            &["-p", "hi", "--config", missing.to_str().unwrap()],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2);
    assert!(
        !missing.exists(),
        "an explicit missing path is never created"
    );
}

/// `--export` validates the id as a session id before using it as a file name: a path-like
/// value must not be able to read a `.jsonl` outside the session directory. The forged file
/// one level up claims the current workspace, so the workspace check alone would let it
/// through; the id validation is what refuses it.
#[tokio::test]
async fn export_refuses_a_path_like_session_id() {
    let dir = scratch("export-path-id");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // `dir/x.jsonl` is `sessions/../x.jsonl`; its workspace is the current one on purpose.
    write_session(
        &dir,
        "x",
        &canonical(&ws),
        "OUTSIDE-THE-SESSION-DIR",
        None,
        None,
    );
    let base = [
        "--model",
        "m",
        "--session-dir",
        sessions.to_str().unwrap(),
        "--workspace",
        ws.to_str().unwrap(),
    ];
    for bad in ["../x", "a/b", ".."] {
        let mut args = base.to_vec();
        args.extend_from_slice(&["--export", bad]);
        let (code, stdout, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "{bad:?}: {stderr}");
        assert!(stderr.contains("session id"), "{bad:?}: {stderr}");
        assert!(stdout.is_empty(), "{bad:?}: {stdout}");
        assert!(
            !stderr.contains("OUTSIDE-THE-SESSION-DIR"),
            "{bad:?}: the error leaked file content: {stderr}"
        );
    }
}

/// An export does not know which provider wrote the session, so without `--api-key-env` both
/// providers' default key variables are named secrets. The key here has no recognizable
/// pattern (unlike `KEY`'s `sk-` prefix), so only a named secret can remove it.
#[tokio::test]
async fn export_redacts_the_anthropic_key_without_api_key_env() {
    let opaque = "opaque-anthropic-credential-7f3a";
    let dir = scratch("export-anthropic-secret");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "anth",
        &canonical(&ws),
        &format!("the anthropic key {opaque} appears here"),
        None,
        None,
    );
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--export",
                "anth",
            ],
            &[("ANTHROPIC_API_KEY", opaque)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains(opaque),
        "the anthropic key leaked: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

// ---------- small fixes: existing session id, provider flags, scratch cleanup ----------

/// D15: naming an existing session with `--session-id` points at the command that continues
/// it, and does not touch the file.
#[tokio::test]
async fn session_id_already_exists_points_at_resume() {
    let dir = scratch("dup-id");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("one")]).await;
    let args = [
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        ws.to_str().unwrap(),
        "--session-id",
        "dup",
        "--session-dir",
        sessions.to_str().unwrap(),
        "-p",
        "hi",
    ];
    let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    let before = std::fs::read(sessions.join("dup.jsonl")).unwrap();

    let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("--resume dup"), "{stderr}");
    assert_eq!(
        std::fs::read(sessions.join("dup.jsonl")).unwrap(),
        before,
        "the existing session must not be touched"
    );
}

/// `--thinking` and `--max-tokens` are Anthropic-only: under the default openai-compatible
/// provider they are a usage error before any session file exists, not a silent no-op.
#[tokio::test]
async fn anthropic_only_flags_are_refused_under_openai_compatible() {
    // `--thinking` left this check in ADR 0026 D11: the unified levels ride both wire
    // protocols (`reasoning_effort` for OpenAI-compatible), so only `--max-tokens` remains
    // Anthropic-only.
    {
        let (label, extra) = ("max-tokens", vec!["--max-tokens", "100"]);
        let dir = scratch(&format!("flag-{label}"));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let mut args = vec![
            "--model",
            "m",
            "--base-url",
            "http://127.0.0.1:9/v1",
            "--session-dir",
            sessions.to_str().unwrap(),
            "--session-id",
            "flagged",
            "-p",
            "hi",
        ];
        args.extend(extra);
        let (code, stdout, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "{label}: {stderr}");
        assert!(stderr.contains("anthropic"), "{label}: {stderr}");
        assert!(stdout.is_empty(), "{label}: {stdout}");
        assert!(
            std::fs::read_dir(&sessions).unwrap().next().is_none(),
            "{label}: a session file was created"
        );
    }
}

/// The same flags work under `--provider anthropic`: `--max-tokens` reaches the body and
/// `--thinking adaptive` becomes the `thinking` object.
#[tokio::test]
async fn anthropic_provider_accepts_thinking_and_max_tokens() {
    let server = anth::MockServer::start(vec![anth_text_reply("THINK-OK")]).await;
    let dir = scratch("anthropic-flags");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_in(
            &dir,
            &[
                "--model",
                "claude-x",
                "--provider",
                "anthropic",
                "--thinking",
                "adaptive",
                "--max-tokens",
                "4096",
                "-p",
                "hi",
            ],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_BASE_URL", &server.base_url),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("THINK-OK"), "{stdout}");
    let body = server.requests()[0].json();
    assert_eq!(body["max_tokens"], 4096, "{body}");
    assert_eq!(body["thinking"], json!({"type": "adaptive"}), "{body}");
}

/// D14: a scratch directory is removed when its guard drops, so a passing test leaves
/// nothing behind under `TMPDIR`.
#[test]
fn scratch_directories_are_removed_when_the_guard_drops() {
    let path = {
        let dir = scratch("scratch-drop");
        assert!(dir.exists(), "{}", dir.display());
        dir.to_path_buf()
    };
    assert!(
        !path.exists(),
        "the scratch guard must remove {}",
        path.display()
    );
}

/// ADR 0016: the tier names are `workspace`/`full`; the old names stay accepted as
/// aliases but are not advertised in `--help`. The removed `safe` tier is rejected.
#[tokio::test]
async fn tier_names_and_aliases_are_accepted_but_help_lists_only_new_names() {
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&["--help"], &[]), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("workspace"), "{stdout}");
    assert!(stdout.contains("full"), "{stdout}");
    assert!(
        !stdout.contains("safe"),
        "the removed tier must not be advertised: {stdout}"
    );
    assert!(
        !stdout.contains("workspace-write"),
        "the old name must not be advertised: {stdout}"
    );
    assert!(
        !stdout.contains("full-access"),
        "the old name must not be advertised: {stdout}"
    );

    for profile in ["workspace", "full", "workspace-write", "full-access"] {
        let dir = scratch(&format!("alias-{profile}"));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--profile",
                    profile,
                    "--session-dir",
                    sessions.to_str().unwrap(),
                    "--list",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{profile}: {stderr}");
    }
}

/// A new session directory and file are owner-only (ADR 0013): a session is potentially
/// sensitive data (ADR 0006 §11).
#[cfg(unix)]
#[tokio::test]
async fn a_new_session_directory_and_file_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = scratch("private-session");
    let sessions = dir.join("sessions"); // does not exist yet
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &sessions,
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "priv",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(mode(&sessions), 0o700, "the session directory must be 0700");
    assert_eq!(
        mode(&sessions.join("priv.jsonl")),
        0o600,
        "the session file must be 0600"
    );
}

/// A session directory that already exists with wider permissions is reported on stderr and
/// left exactly as it was: the fix never silently changes a user's directory.
#[cfg(unix)]
#[tokio::test]
async fn a_wide_session_directory_is_warned_about_and_not_changed() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch("wide-session-dir");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_in(
            &sessions,
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[privacy]"),
        "no warning on stderr: {stderr}"
    );
    assert!(stderr.contains("chmod 700"), "{stderr}");
    assert_eq!(
        std::fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
        0o755,
        "an existing directory's permissions must not be changed"
    );
}

/// The configured endpoint can carry credentials in its userinfo or a query token. Any error
/// that names it must redact them (ADR 0013 T4), and the host must not prepend the raw value.
#[tokio::test]
async fn a_base_url_with_credentials_is_redacted_in_errors() {
    // No key + a remote endpoint: the "no API key" error names the endpoint.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                "https://user:url-secret@example.com/v1?token=query-secret",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(!stderr.contains("url-secret"), "userinfo leaked: {stderr}");
    assert!(
        !stderr.contains("query-secret"),
        "query token leaked: {stderr}"
    );
    assert!(stderr.contains("example.com"), "{stderr}");

    // An unparseable URL with a secret: the provider error names the redacted form, and the
    // host must not prepend the raw one.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", "url-secret", "-p", "hi"],
            &[("OPENAI_API_KEY", "sk-x")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(
        !stderr.contains("url-secret"),
        "the raw URL leaked: {stderr}"
    );
}

/// A search snippet is session content and can carry a model-echoed key; `--search` redacts
/// the known secrets exactly like `--export` (ADR 0013 T3).
#[tokio::test]
async fn search_redacts_the_key_from_a_snippet() {
    let dir = scratch("search-redact");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("echoed sk-search-secret in the answer")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "s1",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "find me",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--workspace",
                ws.to_str().unwrap(),
                "--search",
                "echoed",
            ],
            &[("OPENAI_API_KEY", "sk-search-secret")],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stdout.contains("sk-search-secret"),
        "the key leaked into a search hit: {stdout}"
    );
    assert!(stdout.contains("<redacted>"), "{stdout}");
}

// ---------- model errors end the turn, not the session (ADR 0017) ----------

/// Interactive mode: a 500 is shown on stderr and the prompt comes back; the next line runs a
/// new turn that succeeds. The driver is not ended by the model error.
#[tokio::test]
async fn an_interactive_model_error_is_shown_and_the_session_continues() {
    let server = MockServer::start(vec![
        // A non-transient status (ADR 0017 retries only connection failures, dropped
        // streams, idle timeouts and 408/429/5xx/529), so one failure ends the turn.
        Reply::status(400, "provider is down"),
        text_reply("SECOND-OK"),
    ])
    .await;
    let mut cli = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "recover",
        ],
        &[("OPENAI_API_KEY", KEY)],
    );
    cli.wait_for_stderr("> ").await;
    cli.write("first\n").await;
    // The error text reaches stderr (masked by the adapter and the host).
    cli.wait_for_stderr("HTTP 400").await;
    cli.wait_for_stderr("provider is down").await;
    // The prompt is back: the session did not exit.
    cli.wait_for_stderr_count("> ", 2).await;
    cli.write("second\n").await;
    cli.wait_for_stderr_count("> ", 3).await;
    cli.write("/exit\n").await;
    let (code, stdout, stderr) = cli
        .finish_open(
            Duration::from_secs(5),
            "/exit was written after the recovered turn; stdin stayed open",
        )
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SECOND-OK"), "{stdout}");
    assert_eq!(server.requests().len(), 2, "both turns reached the model");
}

/// ADR 0017: a transient error (HTTP 500) is retried inside the one model call, the
/// retry line goes to stderr, and the turn still succeeds on the second attempt.
#[tokio::test]
async fn a_transient_error_is_retried_and_the_turn_still_succeeds() {
    let server = MockServer::start(vec![
        Reply::status(500, "provider is down"),
        text_reply("RETRIED-OK"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("RETRIED-OK"), "{stdout}");
    assert!(stderr.contains("retrying 1/3"), "{stderr}");
    assert_eq!(
        server.requests().len(),
        2,
        "the first attempt and its retry reached the model"
    );
}

/// One-shot mode is unchanged: a model error still exits 1, and the message is on stderr.
#[tokio::test]
async fn one_shot_model_error_still_exits_one_and_shows_the_error() {
    let server = MockServer::start(vec![Reply::status(400, "provider is down")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("HTTP 400"), "{stderr}");
    assert!(stdout.is_empty(), "stdout must stay model-only: {stdout}");
}

/// ADR 0018's fallback: a recognizable context-overflow error is classified as such and gets
/// one extra hint line (the automatic compaction is a later slice); it still exits 1.
#[tokio::test]
async fn a_context_overflow_error_adds_a_hint() {
    let body = r#"{"error":{"message":"This model's maximum context length is 16385 tokens. However, your messages resulted in 20000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
    let server = MockServer::start(vec![Reply::status(400, body)]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("OPENAI_API_KEY", KEY)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("model context overflow"), "{stderr}");
    assert!(
        stderr.contains("compaction could not keep the request"),
        "the hint must be shown: {stderr}"
    );
    assert!(stdout.is_empty(), "stdout must stay model-only: {stdout}");
}

// ---------- model catalog: the startup window line (ADR 0018) ----------

const CATALOG_JSON: &str = r#"{
  "opencode": {
    "id": "opencode",
    "models": {
      "deepseek-v4.1-flash": {"id": "deepseek-v4.1-flash",
        "limit": {"context": 1000000, "input": 922000, "output": 384000}}
    }
  }
}"#;

/// The explicit window is named on startup and wins over the catalog. ADR 0018 shows `C`, `I`,
/// `output`, `R`, and the effective input budget `B` separately.
#[tokio::test]
async fn the_startup_line_names_an_explicit_window() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    // C = 20000 -> R = 5000 (a quarter of the window), B = 15000.
    assert!(stderr.contains("B 15000 from --context-window"), "{stderr}");
    assert!(stderr.contains("R 5000"), "{stderr}");
}

/// `--no-auto-compact` is shown on the startup line; the default is on (ADR 0018).
#[tokio::test]
async fn the_startup_line_names_compaction_state() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "--no-auto-compact",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("automatic compaction off"), "{stderr}");

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--context-window",
                "20000",
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("automatic compaction on"), "{stderr}");
}

/// With no window and an empty cache, there is no budget and the startup says so.
#[tokio::test]
async fn the_startup_line_is_unknown_without_a_window() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let home = scratch("no-window-home");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--offline-catalog",
                "-p",
                "hi",
            ],
            &[("HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[budget] window unknown"), "{stderr}");
    assert!(stderr.contains("no budget"), "{stderr}");
}

/// The catalog line names the provider/model and the numbers, and the numbers show separately.
#[tokio::test]
async fn the_startup_line_names_the_catalog() {
    let provider = MockServer::start(vec![text_reply("ok")]).await;
    let catalog = MockServer::start(vec![Reply::status(200, CATALOG_JSON)]).await;
    let catalog_url = format!("{}/api.json", catalog.base_url);
    let home = scratch("catalog-home");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "deepseek-v4.1-flash",
                "--base-url",
                &provider.base_url,
                "--catalog-provider",
                "opencode",
                "-p",
                "hi",
            ],
            &[
                ("HOME", home.to_str().unwrap()),
                ("RADIAN_CATALOG_URL", catalog_url.as_str()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("B 850000 from models.dev (opencode/deepseek-v4.1-flash"),
        "{stderr}"
    );
    assert!(stderr.contains("C 1000000"), "{stderr}");
    assert!(stderr.contains("I 922000"), "{stderr}");
    assert!(stderr.contains("output 384000"), "{stderr}");
    // R is the output actually sent: min(384000, max(150000, min(16384, 1000000 / 4)))
    // = 150000.
    assert!(stderr.contains("R 150000"), "{stderr}");
    assert_eq!(
        provider.requests()[0].json()["max_tokens"],
        json!(150000),
        "the catalog cap is sent as max_tokens"
    );
    assert_eq!(catalog.requests().len(), 1, "the catalog was fetched once");
}

/// ADR 0018 §2: an explicit `max_tokens` from `RADIAN_EXTRA_BODY` wins on the wire, so the
/// reservation counts it instead of the computed chat cap. C = 128000 with O = 384000 would
/// send 19200; the body's 32000 is what the request sends, so R = 32000 and B = 96000.
#[tokio::test]
async fn an_explicit_max_tokens_raises_the_reservation_it_counts() {
    let provider = MockServer::start(vec![text_reply("ok")]).await;
    let home = common::process_dir("radian-exec-override-home");
    let cache = home.join("cache/catalog");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("models.json"), CATALOG_JSON).unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "deepseek-v4.1-flash",
                "--base-url",
                &provider.base_url,
                "--catalog-provider",
                "opencode",
                "--context-window",
                "128000",
                "-p",
                "hi",
            ],
            &[
                ("RADIAN_HOME", home.to_str().unwrap()),
                ("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("output 384000"), "{stderr}");
    assert!(stderr.contains("R 32000"), "{stderr}");
    assert!(stderr.contains("B 96000"), "{stderr}");
    assert_eq!(
        provider.requests()[0].json()["max_tokens"],
        json!(32000),
        "the explicit value is what the request sends"
    );
}

// ---------- ADR 0020: the `~/.radian/` layout ----------

/// Like [`spawn_radian`], but without the default `RADIAN_SESSION_DIR`, so a test controls the
/// session location (`RADIAN_HOME` / `RADIAN_SESSION_DIR` / `--session-dir`).
fn spawn_radian_bare(args: &[&str], envs: &[(&str, &str)]) -> Child {
    let home = fresh_session_dir();
    let root = envs
        .iter()
        .find(|(name, _)| *name == "RADIAN_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .unwrap_or_else(|| home.join(".radian"));
    write_auth_json(&root, envs);
    Command::new(common::radian_bin())
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

/// `RADIAN_HOME` moves the whole root; sessions land in `<RADIAN_HOME>/sessions`.
#[tokio::test]
async fn radian_home_places_sessions_under_the_radian_root() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = scratch("radian-home");
    let radian_home = dir.join("rh");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "h1",
                "-p",
                "hi",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        radian_home.join("sessions/h1.jsonl").exists(),
        "the session must be under <RADIAN_HOME>/sessions"
    );
}

/// `RADIAN_SESSION_DIR` still wins over `RADIAN_HOME` for the sessions directory only.
#[tokio::test]
async fn radian_session_dir_overrides_radian_home() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let dir = scratch("session-dir-wins");
    let radian_home = dir.join("rh");
    let custom = dir.join("custom-sessions");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "h1",
                "-p",
                "hi",
            ],
            &[
                ("RADIAN_HOME", radian_home.to_str().unwrap()),
                ("RADIAN_SESSION_DIR", custom.to_str().unwrap()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        custom.join("h1.jsonl").exists(),
        "RADIAN_SESSION_DIR must win"
    );
    assert!(
        !radian_home.join("sessions/h1.jsonl").exists(),
        "the default must not be used when RADIAN_SESSION_DIR is set"
    );
}

/// ADR 0020 acceptance: under `workspace`, the `shell` tool cannot read the sessions directory;
/// the denial comes back to the model as a tool error and the turn continues.
#[tokio::test]
async fn workspace_shell_cannot_read_the_sessions_directory() {
    let dir = scratch("shell-deny-sessions");
    let radian_home = dir.join("rh");
    let sessions = radian_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("x.jsonl"), "SECRET-SESSION-CONTENT\n").unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/x.jsonl", sessions.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "--session-id",
                "deny",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SECRET-SESSION-CONTENT"),
        "the sandbox leaked the session file: {result}"
    );
    assert!(
        result.contains("exit code:"),
        "a shell result was expected: {result}"
    );
}

/// ADR 0039 decision 1: a background job's output file lives in the session's own
/// `<sessions_dir>/<session-id>.jobs/` directory, which is inside the sessions directory the
/// sandbox denies reading. A sandboxed shell cannot read it.
#[tokio::test]
async fn workspace_shell_cannot_read_a_background_jobs_output_file() {
    let dir = scratch("shell-deny-jobs");
    let radian_home = dir.join("rh");
    let sessions = radian_home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let session_id = "deny-jobs";
    let jobs = sessions.join(format!("{session_id}.jobs"));
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("job-1.out"), "SECRET-JOB-OUTPUT\n").unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/job-1.out", jobs.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "--session-id",
                session_id,
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SECRET-JOB-OUTPUT"),
        "the sandbox leaked a background job's output file: {result}"
    );
    assert!(
        result.contains("exit code:"),
        "a shell result was expected: {result}"
    );
}

/// ADR 0020 acceptance 2: a `--session-dir` that does not exist yet must still be denied. The
/// host creates it 0700 **before** building the sandbox, because Linux's bwrap can only mask
/// paths that already exist; without that, the session file (created after the sandbox) would
/// be readable. This is the case that only fails on Linux if the create-before-deny is missing.
#[tokio::test]
async fn workspace_shell_cannot_read_a_moved_sessions_directory() {
    let dir = scratch("shell-deny-moved");
    let radian_home = dir.join("rh");
    let custom = dir.join("custom-sessions"); // does not exist yet
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();

    let command = format!("cat {}/moved.jsonl", custom.display());
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({ "command": command }).to_string()),
        text_reply("SAW-DENIED"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_bare(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--yes",
                "--session-dir",
                custom.to_str().unwrap(),
                "--session-id",
                "moved",
                "-p",
                "go",
            ],
            &[("RADIAN_HOME", radian_home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("SAW-DENIED"), "{stdout}");
    let result = tool_result_content(&server, 1);
    assert!(
        !result.contains("SessionCreated"),
        "the sandbox read the session file: {result}"
    );
}

// ---------- `radian memory` (ADR 0021 D7/D8) ----------

/// An isolated `radian memory` environment: a workspace, a `RADIAN_HOME`, and a sessions
/// directory that must stay free of `.jsonl` files.
struct MemoryEnv {
    dir: ScratchDir,
    ws: PathBuf,
    home: PathBuf,
    sessions: PathBuf,
}

impl MemoryEnv {
    fn new(name: &str) -> Self {
        let dir = ScratchDir(common::process_dir(&format!("radian-exec-memory-{name}")));
        Self::with_dir(dir)
    }

    /// Like [`MemoryEnv::new`], under `$TMPDIR` (which the workspace sandbox can write), for a
    /// test that must run inside it: `/tmp` is denied there.
    fn sandboxed(name: &str) -> Self {
        let dir = ScratchDir(common::process_dir(&format!("radian-exec-{name}")));
        Self::with_dir(dir)
    }

    fn with_dir(dir: ScratchDir) -> Self {
        let ws = dir.join("ws");
        let home = dir.join("radian");
        let sessions = dir.join("sessions");
        for path in [&ws, &home, &sessions] {
            std::fs::create_dir_all(path).unwrap();
        }
        Self {
            dir,
            ws,
            home,
            sessions,
        }
    }

    /// A second workspace under the same scratch (a moved repository, a worktree).
    fn workspace(&self, name: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    async fn run(&self, args: &[&str]) -> (i32, String, String) {
        self.run_in(&self.ws, args).await
    }

    async fn run_in(&self, ws: &Path, args: &[&str]) -> (i32, String, String) {
        self.run_with_input_in(ws, args, "").await
    }

    async fn run_with_input_in(
        &self,
        ws: &Path,
        args: &[&str],
        input: &str,
    ) -> (i32, String, String) {
        // The top-level `radian memory --workspace … <command>` (ADR 0032:
        // `radian memory` and `radian exec memory` run the same code).
        let mut full: Vec<String> = vec![
            "memory".into(),
            "--workspace".into(),
            ws.to_string_lossy().into_owned(),
        ];
        full.extend(args.iter().map(|arg| (*arg).to_owned()));
        let refs: Vec<&str> = full.iter().map(String::as_str).collect();
        let envs = [
            ("RADIAN_HOME", self.home.to_str().unwrap()),
            ("RADIAN_SESSION_DIR", self.sessions.to_str().unwrap()),
        ];
        feed_and_wait(spawn_radian_top(&refs, &envs), input).await
    }

    /// The directory of the only project in this home.
    #[allow(dead_code)]
    fn project_dir(&self) -> PathBuf {
        let projects = self.home.join("memory/projects");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        dirs.sort();
        assert_eq!(dirs.len(), 1, "expected one project, found {dirs:?}");
        dirs[0].clone()
    }
}

/// Writes one memory record file by hand: the store's own writer always stamps `created` and
/// `modified` with now, and the aging tests need controlled timestamps.
fn write_memory_record(
    dir: &Path,
    name: &str,
    last_used: Option<&str>,
    created: &str,
    description: &str,
) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let last = last_used
        .map(|value| format!("last_used: {value}\n"))
        .unwrap_or_default();
    let path = dir.join(format!("{name}.md"));
    std::fs::write(
        &path,
        format!(
            "---\nname: {name}\ndescription: {description}\ntype: project\nsource: model\n\
             created: {created}\nmodified: {created}\n{last}---\nthe body\n"
        ),
    )
    .unwrap();
    path
}

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A memory command needs no `--model`, reads no key and creates no session file (ADR 0021
/// D7). `--workspace` (a top-level flag, before `memory`) is enough.
#[tokio::test]
async fn memory_commands_do_not_need_a_model_or_create_a_session() {
    let env = MemoryEnv::new("mem-no-model");
    let (code, stdout, stderr) = env.run(&["project"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("identity:"), "{stdout}");
    assert!(!stderr.contains("--model"), "{stderr}");
    assert!(
        jsonl_snapshot(&env.sessions).is_empty(),
        "a memory command created a session file"
    );
}

#[tokio::test]
async fn memory_add_list_show_edit_round_trip() {
    let env = MemoryEnv::new("mem-roundtrip");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");

    let (code, stdout, stderr) = env.run(&["add", "--type", "feedback", "style"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("added style"), "{stdout}");
    assert!(
        stdout.contains("no editor was opened"),
        "off a terminal, add says so: {stdout}"
    );

    // The file exists with the skeleton: source user, created/modified, empty body.
    let project = linked_project(&env, &env.ws).await;
    let path = env
        .home
        .join("memory/projects")
        .join(&project)
        .join("style.md");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("source: user"), "{text}");
    assert!(text.contains("type: feedback"), "{text}");
    assert!(text.contains("created:"), "{text}");
    assert!(text.contains("modified:"), "{text}");
    let index = std::fs::read_to_string(path.with_file_name("MEMORY.md")).unwrap();
    assert!(index.contains("[style](style.md)"), "{index}");

    let (code, stdout, stderr) = env.run(&["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("project\tstyle\tfeedback\tuser"),
        "{stdout}"
    );

    let (code, stdout, _) = env.run(&["show", "style"]).await;
    assert_eq!(code, 0);
    assert!(
        stdout.contains("name: style"),
        "show prints the file as stored: {stdout}"
    );

    let (code, stdout, stderr) = env.run(&["list", "--json"]).await;
    assert_eq!(code, 0, "{stderr}");
    let value: Value = serde_json::from_str(stdout.trim()).expect("list --json is JSON");
    let record = &value.as_array().unwrap()[0];
    assert_eq!(record["name"], "style");
    assert_eq!(record["type"], "feedback");
    assert_eq!(record["source"], "user");
    assert!(record["created"].is_string());
    assert!(record["modified"].is_string());

    // `edit` without an editor on a pipe prints the path and says no editor ran.
    let (code, stdout, stderr) = env.run(&["edit", "style"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("style.md"), "{stdout}");
    assert!(stdout.contains("no editor was opened"), "{stdout}");
}

#[tokio::test]
async fn memory_add_refuses_a_bad_name_before_registering_an_unlinked_git_project() {
    let env = MemoryEnv::new("mem-add-bad-name-unlinked-git");
    // A `.git` directory makes the workspace a project workspace that `add` would otherwise
    // register automatically. The bad name must be refused before that write.
    std::fs::create_dir_all(env.ws.join(".git")).unwrap();

    let (code, stdout, stderr) = env.run(&["add", "Bad Name"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("a-z, 0-9 and -"), "{stderr}");
    assert!(
        !stdout.contains("created project"),
        "a refused name printed the created-project notice: {stdout}"
    );
    assert!(
        !env.home.join("memory/projects.jsonl").exists(),
        "a refused name registered a project"
    );
    assert!(
        !env.home.join("memory/projects").exists(),
        "a refused name created a project directory"
    );
}

#[tokio::test]
async fn memory_add_refuses_a_bad_name_before_an_unlinked_workspace_refusal() {
    let env = MemoryEnv::new("mem-add-bad-name-unlinked");
    let (code, _, stderr) = env.run(&["add", "Bad Name"]).await;
    assert_eq!(
        code, 2,
        "the name rule wins over the workspace refusal: {stderr}"
    );
    assert!(stderr.contains("a-z, 0-9 and -"), "{stderr}");
    assert!(
        !stderr.contains("not a project workspace"),
        "a bad name is a usage error, not a project refusal: {stderr}"
    );
}

/// Edge 5: `split` reports a name already present in the target and moves neither file.
#[tokio::test]
async fn memory_split_reports_a_collision_and_moves_nothing() {
    let env = MemoryEnv::new("mem-split-collision");
    let a = env.workspace("split-a");
    let b = env.workspace("split-b");
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let (code, _, stderr) = env.run_in(&a, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run_in(&b, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (_, _, stderr) = env.run_in(&a, &["add", "note"]).await;
    assert_eq!(stderr, "", "{stderr}");
    let (_, _, stderr) = env.run_in(&b, &["add", "note"]).await;
    assert_eq!(stderr, "", "{stderr}");
    let project_a = linked_project(&env, &a).await;
    let project_b = linked_project(&env, &b).await;

    let (code, out, stderr) = env
        .run_in(&a, &["split", "--yes", &project_a, &project_b, "note"])
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        out.contains("conflict: note exists in the target; it was not moved"),
        "the collision is reported: {out}"
    );
    let (_, out, _) = env.run_in(&a, &["list"]).await;
    assert!(out.contains("note"), "the source file stays: {out}");
    let (_, out, _) = env.run_in(&b, &["list"]).await;
    assert_eq!(
        out.lines().filter(|line| line.contains("\tnote\t")).count(),
        1,
        "the target copied nothing: {out}"
    );
}

#[tokio::test]
async fn memory_add_refuses_a_bad_or_existing_name_and_writes_nothing() {
    let env = MemoryEnv::new("mem-add-refusal");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let (code, _, stderr) = env.run(&["add", "Bad Name"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("a-z, 0-9 and -"), "{stderr}");

    let (code, _, stderr) = env.run(&["add", "note"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run(&["add", "note"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    let (_, stdout, _) = env.run(&["list"]).await;
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
}

#[tokio::test]
async fn memory_forget_needs_confirmation_off_a_terminal() {
    let env = MemoryEnv::new("mem-forget");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let (_, _, _) = env.run(&["add", "keep"]).await;

    let (code, _, stderr) = env.run(&["forget", "keep"]).await;
    assert_eq!(code, 2, "non-tty without --yes refuses: {stderr}");
    assert!(stderr.contains("--yes"), "{stderr}");
    let (code, _, _) = env.run(&["show", "keep"]).await;
    assert_eq!(code, 0, "the file is still there");

    let (code, stdout, stderr) = env.run(&["forget", "--yes", "keep"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("forgot"), "{stdout}");
    let (code, _, _) = env.run(&["show", "keep"]).await;
    assert_eq!(code, 1, "the file is gone");
}

#[tokio::test]
async fn the_removed_memory_commands_exit_two_and_name_the_current_set() {
    let env = MemoryEnv::new("mem-removed");
    for command in ["purge", "export", "confirm"] {
        let (code, _, stderr) = env.run(&[command]).await;
        assert_eq!(code, 2, "{command}: {stderr}");
        assert!(
            stderr.contains(
                "list, show, stale, add, edit, forget, open, project, relink, split, merge"
            ),
            "{command}: {stderr}"
        );
        assert!(stderr.contains("were removed"), "{command}: {stderr}");
    }
}

#[tokio::test]
async fn memory_open_off_a_terminal_prints_the_directory_and_exits_zero() {
    let env = MemoryEnv::new("mem-open");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let (code, stdout, stderr) = env.run(&["open"]).await;
    assert_eq!(code, 0, "{stderr}");
    let expected = env.home.join("memory/projects").join(project);
    assert_eq!(stdout.trim(), expected.to_string_lossy(), "{stdout}");
}

#[tokio::test]
async fn memory_show_of_a_bad_file_names_the_line_and_list_still_lists_the_others() {
    let env = MemoryEnv::new("mem-bad-frontmatter");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let (_, _, _) = env.run(&["add", "good"]).await;
    let project = linked_project(&env, &env.ws).await;
    let bad = env
        .home
        .join("memory/projects")
        .join(&project)
        .join("bad.md");
    std::fs::write(
        &bad,
        "---\nname: bad\ndescription: x\ntype: nope\nsource: user\ncreated: 2026-10-31T12:00:00.000000Z\nmodified: 2026-10-31T12:00:00.000000Z\n---\nbody\n",
    )
    .unwrap();

    let (code, _, stderr) = env.run(&["show", "bad"]).await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("line 4"),
        "the error names the line: {stderr}"
    );
    let (code, stdout, stderr) = env.run(&["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("good"), "{stdout}");
    assert!(stdout.contains("unreadable"), "{stdout}");
}

#[tokio::test]
async fn memory_json_output_is_parseable_and_has_stable_fields() {
    let env = MemoryEnv::new("mem-json");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let (_, _, _) = env.run(&["add", "json-note", "--type", "reference"]).await;

    let (code, stdout, stderr) = env.run(&["list", "--json"]).await;
    assert_eq!(code, 0, "{stderr}");
    let value: Value = serde_json::from_str(stdout.trim()).expect("list --json is JSON");
    let record = &value.as_array().unwrap()[0];
    for field in [
        "scope",
        "name",
        "description",
        "type",
        "source",
        "created",
        "modified",
    ] {
        assert!(record.get(field).is_some(), "missing {field}: {record}");
    }
    assert_eq!(record["name"], "json-note");
    assert_eq!(record["type"], "reference");
    assert_eq!(record["source"], "user");
    assert_eq!(record["scope"], "project");

    let (code, stdout, _) = env.run(&["show", "json-note", "--json"]).await;
    assert_eq!(code, 0);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["frontmatter"]["name"], "json-note");
    assert_eq!(value["body"], "");

    let (code, stdout, _) = env.run(&["project", "--json"]).await;
    assert_eq!(code, 0);
    let value: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["state"], "linked");
    assert!(value["project_id"].is_string());
    assert!(value["identity_key"].is_string());
}

#[tokio::test]
async fn memory_unlinked_workspace_refuses_project_writes_and_lists_global() {
    let env = MemoryEnv::new("mem-unlinked");
    let (code, _, stderr) = env.run(&["add", "--global", "global-note"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, stdout, stderr) = env.run(&["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("global-note"), "{stdout}");
    assert!(
        stderr.contains("not linked"),
        "a default-scope list says the workspace is unlinked: {stderr}"
    );
    assert!(
        stderr.contains("no candidate projects"),
        "no candidate projects yet: {stderr}"
    );

    let (code, _, stderr) = env.run(&["add", "project-note"]).await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("not a project workspace"),
        "the workspace is not a git repository, so it is not a project workspace: {stderr}"
    );
    assert!(
        stderr.contains("not saved"),
        "the refusal says the memory was not saved: {stderr}"
    );
    assert!(
        !stderr.contains("--global"),
        "the refusal must not point at another scope: {stderr}"
    );
    assert!(
        !env.home.join("memory/projects").exists(),
        "a refused Project write created a project directory"
    );

    let (code, _, _) = env.run(&["list", "--project"]).await;
    assert_eq!(code, 1, "a Project-only read needs a project");
}

/// Edge 4: a user command never touches `last_used`. `show` reads the record store, not the
/// memory files, and the file's bytes stay exactly as they were.
#[tokio::test]
async fn memory_show_does_not_touch_last_used() {
    let env = MemoryEnv::sandboxed("mem-show-last-used");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let dir = env.home.join("memory/projects").join(&project);
    let path = write_memory_record(
        &dir,
        "note",
        Some("2020-01-01T00:00:00.000000Z"),
        "2020-01-01T00:00:00.000000Z",
        "a note",
    );
    let before = std::fs::read_to_string(&path).unwrap();

    let (code, stdout, stderr) = env.run(&["show", "--project", "note"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("the body"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        before,
        "`show` changed the memory file"
    );
}

/// Edges 5, 6, 8 and 9: `stale` lists old `last_used`, never-read files by `created`, carries
/// the frontmatter fields in JSON, and reports a malformed file as unreadable beside the rest.
#[tokio::test]
async fn memory_stale_lists_aged_files_and_speaks_json() {
    let env = MemoryEnv::sandboxed("mem-stale");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let dir = env.home.join("memory/projects").join(&project);
    let old = "2020-01-01T00:00:00.000000Z";
    let now = radian_memory::now_utc();
    write_memory_record(&dir, "old-read", Some(old), old, "read long ago");
    write_memory_record(&dir, "recent-read", Some(&now), old, "read just now");
    write_memory_record(
        &dir,
        "unread-old",
        None,
        old,
        "never read, created long ago",
    );
    write_memory_record(
        &dir,
        "unread-new",
        None,
        &now,
        "never read, created just now",
    );
    write_memory_record(
        &dir,
        "broken",
        Some("not-a-timestamp"),
        old,
        "whose timestamp is malformed",
    );

    // Edge 5: `--days 7` lists the 2020 read and not the one read just now.
    let (code, stdout, stderr) = env.run(&["stale", "--project", "--days", "7"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("old-read"), "{stdout}");
    assert!(!stdout.contains("read just now"), "{stdout}");
    // Edge 6: the default 30 days judges a never-read file by `created`.
    let (code, stdout, stderr) = env.run(&["stale", "--project"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("unread-old"), "{stdout}");
    assert!(stdout.contains("never"), "{stdout}");
    assert!(!stdout.contains("created just now"), "{stdout}");
    // Edge 9: the broken file is named as unreadable, and the others still list.
    assert!(stdout.contains("broken.md"), "{stdout}");
    assert!(stdout.contains("unreadable"), "{stdout}");
    assert!(stdout.contains("read long ago"), "{stdout}");

    // Edge 8: `--json` parses and carries the frontmatter fields.
    let (code, stdout, stderr) = env.run(&["stale", "--project", "--json"]).await;
    assert_eq!(code, 0, "{stderr}");
    let value: Value = serde_json::from_str(stdout.trim()).expect("stale --json is JSON");
    let records = value.as_array().unwrap();
    let old_read = records
        .iter()
        .find(|record| record["name"] == "old-read")
        .unwrap_or_else(|| panic!("no old-read record: {records:#?}"));
    assert_eq!(old_read["scope"].as_str().unwrap(), "project");
    assert_eq!(old_read["last_used"], old);
    assert_eq!(old_read["created"], old);
    assert_eq!(old_read["description"], "read long ago");
    let unread = records
        .iter()
        .find(|record| record["name"] == "unread-old")
        .unwrap();
    assert_eq!(unread["last_used"], Value::Null);

    // Edge 7: `--days 0` is a usage error (exit 2) naming the minimum.
    let (code, _, stderr) = env.run(&["stale", "--days", "0"]).await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("at least 1"), "{stderr}");
}

/// Edge 1 end to end: a scripted model `read`s a project memory file through the tool; the
/// host records `last_used` within the test's window and changes no other byte.
#[tokio::test]
async fn a_model_read_of_a_project_memory_file_records_last_used() {
    let env = MemoryEnv::sandboxed("mem-read-last-used");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let dir = env.home.join("memory/projects").join(&project);
    let path = write_memory_record(
        &dir,
        "note",
        Some("2020-01-01T00:00:00.000000Z"),
        "2020-01-01T00:00:00.000000Z",
        "a note",
    );
    let before = std::fs::read_to_string(&path).unwrap();
    let target = serde_json::to_string(path.to_str().unwrap()).unwrap();
    let server = MockServer::start(vec![
        tool_reply("r1", "read", &format!("{{\"path\":{target}}}")),
        text_reply("done"),
    ])
    .await;

    let start = radian_memory::now_utc();
    let envs = [
        ("RADIAN_HOME", env.home.to_str().unwrap()),
        ("RADIAN_SESSION_DIR", env.sessions.to_str().unwrap()),
    ];
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--profile",
                "workspace",
                "--session-id",
                "read-last-used",
                "--yes",
                "-p",
                "read the note",
            ],
            &envs,
        ),
        "",
    )
    .await;
    let end = radian_memory::now_utc();
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert!(
        tool_result_content(&server, 1).contains("the body"),
        "the read reached the model"
    );

    let after = std::fs::read_to_string(&path).unwrap();
    let new_line = after
        .lines()
        .find(|line| line.starts_with("last_used:"))
        .unwrap_or_else(|| panic!("no last_used line in {after}"))
        .to_owned();
    let value = new_line.strip_prefix("last_used: ").unwrap();
    assert!(
        start.as_str() <= value && value <= end.as_str(),
        "last_used {value} is not within [{start}, {end}]"
    );
    let old_line = "last_used: 2020-01-01T00:00:00.000000Z";
    assert_eq!(
        after.replace(&new_line, old_line),
        before,
        "every other byte is unchanged"
    );
}

/// ADR 0021: a default-scope `radian memory add` in a git workspace
/// whose project was never registered **registers it**, prints one notice line naming the new
/// project and its identity key, and saves into that project. The next add is silent.
#[tokio::test]
async fn memory_add_registers_a_git_workspace_and_prints_one_notice() {
    if !git_available() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let env = MemoryEnv::new("mem-auto-create");
    git(&env.ws, &["init", "-q"]);

    let (code, _, stderr) = env.run(&["add", "project-note"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[memory] created project"),
        "one notice line names the new project: {stderr}"
    );
    assert!(
        stderr.contains("identity key"),
        "the notice names the identity key: {stderr}"
    );
    assert_eq!(
        stderr.lines().count(),
        1,
        "one notice line, not a paragraph: {stderr}"
    );
    let project = linked_project(&env, &env.ws).await;
    assert!(
        stderr.contains(&project),
        "the notice names the project it created: {stderr}"
    );
    // The file went into that project.
    let (code, stdout, stderr) = env.run(&["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("project\tproject-note"),
        "the file is a project record: {stdout}"
    );
    let registry = std::fs::read_to_string(env.home.join("memory/projects.jsonl")).unwrap();
    assert_eq!(
        registry
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "projects.jsonl gained the one project: {registry}"
    );

    // Already linked: the second add says nothing about projects.
    let (code, _, stderr) = env.run(&["add", "second-note"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.is_empty(),
        "no notice for a linked project: {stderr}"
    );

    // A read never registers anything: the same commands in a second, unregistered repository
    // leave the registry alone.
    let other = env.workspace("other");
    git(&other, &["init", "-q"]);
    let (code, stdout, stderr) = env.run_in(&other, &["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("project-note"), "{stdout}");
    let after = std::fs::read_to_string(env.home.join("memory/projects.jsonl")).unwrap();
    assert_eq!(after, registry, "a read registered a project");
}

#[tokio::test]
async fn memory_relink_after_a_move_finds_the_original_records() {
    let env = MemoryEnv::new("mem-relink");
    let first = env.workspace("here/radian");
    let (code, _, stderr) = env.run_in(&first, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (_, _, stderr) = env.run_in(&first, &["add", "kept"]).await;
    assert_eq!(stderr, "");
    let (_, project_out, _) = env.run_in(&first, &["project"]).await;
    let project = project_out
        .lines()
        .find_map(|line| line.trim().strip_prefix("state: linked to "))
        .expect("a linked project")
        .to_owned();

    let second = env.workspace("moved/radian");
    let (code, stdout, stderr) = env.run_in(&second, &["project"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("not linked"), "{stdout}");
    assert!(
        stdout.contains(&project),
        "the candidate is listed: {stdout}"
    );
    let (code, _, _) = env.run_in(&second, &["add", "nope"]).await;
    assert_eq!(code, 1);

    let (code, _, stderr) = env.run_in(&second, &["relink", &project]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, stdout, stderr) = env.run_in(&second, &["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("kept"), "{stdout}");
}

/// A real `git worktree` resolves to the main repository's project (ADR 0021 D2). Skipped
/// without `git`, with the reason printed.
#[tokio::test]
async fn memory_project_is_shared_with_a_git_worktree() {
    if !git_available() {
        eprintln!("skipping: git is not on PATH");
        return;
    }
    let env = MemoryEnv::new("mem-worktree");
    let main = env.workspace("main");
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "t@example.invalid"]);
    git(&main, &["config", "user.name", "t"]);
    std::fs::write(main.join("a.txt"), "hi\n").unwrap();
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "init"]);
    let wt = env.workspace("wt");
    std::fs::remove_dir(&wt).unwrap();
    git(
        &main,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wtb"],
    );

    let (code, _, stderr) = env.run_in(&main, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (_, _, _) = env.run_in(&main, &["add", "shared"]).await;
    let (code, stdout, stderr) = env.run_in(&wt, &["list"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("shared"),
        "a worktree shares the main repository's project: {stdout}"
    );
}

async fn linked_project(env: &MemoryEnv, ws: &Path) -> String {
    let (code, out, err) = env.run_in(ws, &["project"]).await;
    assert_eq!(code, 0, "{err}");
    out.lines()
        .find_map(|line| line.trim().strip_prefix("state: linked to "))
        .unwrap_or_else(|| panic!("not linked: {out}"))
        .to_owned()
}

// ---------- the project session index, `SESSIONS.md` (ADR 0021 D3) ----------

/// A workspace that is a repository without needing `git`: the identity and the repository
/// root only look for `.git`, and these tests run no git command.
fn repository(ws: &Path) {
    std::fs::create_dir_all(ws.join(".git")).unwrap();
}

fn session_index_path(env: &MemoryEnv, project: &str) -> PathBuf {
    env.home
        .join("memory/projects")
        .join(project)
        .join("SESSIONS.md")
}

fn session_index_lines(env: &MemoryEnv, project: &str) -> Vec<String> {
    std::fs::read_to_string(session_index_path(env, project))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn memory_session_envs(env: &MemoryEnv) -> [(&str, &str); 2] {
    [
        ("RADIAN_HOME", env.home.to_str().unwrap()),
        ("RADIAN_SESSION_DIR", env.sessions.to_str().unwrap()),
    ]
}

fn session_started_at(env: &MemoryEnv, id: &str) -> String {
    let session = radian_session::Session::open(env.sessions.join(format!("{id}.jsonl"))).unwrap();
    session.events().first().unwrap().ts.clone()
}

/// Edge case 1 end to end: a session ends and `SESSIONS.md` gains one line whose id, start
/// time and first user line match the journal; the workspace column is `.` at the repository
/// root and `crates/x` in a directory below it.
#[tokio::test]
async fn a_session_end_appends_one_line_with_the_journal_facts() {
    let env = MemoryEnv::sandboxed("mem-sessions-line");
    repository(&env.ws);
    let nested = env.ws.join("crates/x");
    std::fs::create_dir_all(&nested).unwrap();
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;

    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let envs = memory_session_envs(&env);
    for (id, workspace, prompt) in [
        ("root-session", env.ws.as_path(), "hello from root"),
        ("nested-session", nested.as_path(), "hello from nested"),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    workspace.to_str().unwrap(),
                    "--session-id",
                    id,
                    "-p",
                    prompt,
                ],
                &envs,
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
    }

    let lines = session_index_lines(&env, &project);
    assert_eq!(lines.len(), 2, "one line per ended session: {lines:?}");
    assert_eq!(
        lines[0],
        format!(
            "- root-session {} . — hello from root",
            session_started_at(&env, "root-session")
        )
    );
    assert_eq!(
        lines[1],
        format!(
            "- nested-session {} crates/x — hello from nested",
            session_started_at(&env, "nested-session")
        )
    );
}

/// Edge case 2: a session that never commits a user message writes nothing. First a session
/// in the same project writes its line, so the env and the session-end path are proven to work;
/// then a run whose input ends before a turn opens leaves the index byte for byte as it was.
/// A model error after the first step cannot be this case: the Core commits the turn's input
/// before the model call (ADR 0006), so the line would legitimately exist.
#[tokio::test]
async fn a_session_without_a_committed_user_message_appends_nothing() {
    let env = MemoryEnv::sandboxed("mem-sessions-empty");
    repository(&env.ws);
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let envs = memory_session_envs(&env);

    // The same project and the same env do index an ended session that committed a message.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--session-id",
                "has-message",
                "-p",
                "hello",
            ],
            &envs,
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let before = std::fs::read_to_string(session_index_path(&env, &project))
        .expect("the first session wrote the index");
    assert!(before.contains("has-message"), "{before}");
    assert_eq!(session_index_lines(&env, &project).len(), 1, "{before}");

    // A run whose input ends before a turn opens commits no user message at all.
    let server = MockServer::start(Vec::new()).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--session-id",
                "no-message",
            ],
            &envs,
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let session = radian_session::Session::open(env.sessions.join("no-message.jsonl")).unwrap();
    assert!(
        !session
            .events()
            .iter()
            .any(|event| event.event == "MessageAppended"),
        "the session has no committed message"
    );
    assert_eq!(server.requests().len(), 0, "no model call was made");
    let after = std::fs::read_to_string(session_index_path(&env, &project))
        .expect("the first session's index is still there");
    assert_eq!(
        after, before,
        "a session with no committed user message writes nothing"
    );
    assert_eq!(session_index_lines(&env, &project).len(), 1, "{after}");
}

/// Edge case 3: a session in another workspace is another project, so it never appears in
/// this project's file.
#[tokio::test]
async fn another_workspace_session_is_not_in_this_projects_file() {
    let env = MemoryEnv::sandboxed("mem-sessions-project");
    let here = env.workspace("here");
    let there = env.workspace("there");
    repository(&here);
    repository(&there);
    for ws in [&here, &there] {
        let (code, _, stderr) = env.run_in(ws, &["project", "new"]).await;
        assert_eq!(code, 0, "{stderr}");
    }
    let here_project = linked_project(&env, &here).await;
    let there_project = linked_project(&env, &there).await;
    assert_ne!(here_project, there_project);

    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let envs = memory_session_envs(&env);
    for (id, workspace, prompt) in [
        ("here-session", here.as_path(), "here work"),
        ("there-session", there.as_path(), "there work"),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    workspace.to_str().unwrap(),
                    "--session-id",
                    id,
                    "-p",
                    prompt,
                ],
                &envs,
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
    }

    let here_lines = session_index_lines(&env, &here_project);
    let there_lines = session_index_lines(&env, &there_project);
    assert_eq!(here_lines.len(), 1, "{here_lines:?}");
    assert_eq!(there_lines.len(), 1, "{there_lines:?}");
    assert!(here_lines[0].contains("here-session"), "{here_lines:?}");
    assert!(there_lines[0].contains("there-session"), "{there_lines:?}");
}

/// Edge case 4 end to end: a resumed session that ends again replaces its line instead of
/// appending a second one.
#[tokio::test]
async fn a_resumed_session_replaces_its_line() {
    let env = MemoryEnv::sandboxed("mem-sessions-resume");
    repository(&env.ws);
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;

    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let envs = memory_session_envs(&env);
    let base = [
        "--model",
        "m",
        "--base-url",
        server.base_url.as_str(),
        "--workspace",
        env.ws.to_str().unwrap(),
    ];
    let mut first: Vec<&str> = base.to_vec();
    first.extend(["--session-id", "resume-me", "-p", "first task"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&first, &envs), "").await;
    assert_eq!(code, 0, "{stderr}");
    let before = session_index_lines(&env, &project);
    assert_eq!(before.len(), 1, "{before:?}");

    let mut second: Vec<&str> = base.to_vec();
    second.extend(["--resume", "resume-me", "-p", "second task"]);
    let (code, _, stderr) = feed_and_wait(spawn_radian(&second, &envs), "").await;
    assert_eq!(code, 0, "{stderr}");
    let after = session_index_lines(&env, &project);
    assert_eq!(after.len(), 1, "one replaced line, not two: {after:?}");
    assert!(after[0].contains("first task"), "{after:?}");
}

/// Edge case 9: an unlinked workspace writes no file and creates no project. A linked project
/// in the same home indexes its own ended session first, so the env and the session-end path
/// are proven to work; the unlinked run also commits a user message, so the missing line can
/// only be the missing link.
#[tokio::test]
async fn an_unlinked_workspace_writes_no_session_index_and_creates_no_project() {
    let env = MemoryEnv::sandboxed("mem-sessions-unlinked");
    let linked = env.workspace("linked");
    let unlinked = env.workspace("unlinked");
    repository(&linked);
    repository(&unlinked);
    let (code, _, stderr) = env.run_in(&linked, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &linked).await;

    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let envs = memory_session_envs(&env);
    for (id, workspace, prompt) in [
        ("linked-session", linked.as_path(), "linked work"),
        ("unlinked-session", unlinked.as_path(), "unlinked work"),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    workspace.to_str().unwrap(),
                    "--session-id",
                    id,
                    "-p",
                    prompt,
                ],
                &envs,
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{stderr}");
    }

    // The unlinked run had a message to index; only the missing link explains the missing line.
    let unlinked_session =
        radian_session::Session::open(env.sessions.join("unlinked-session.jsonl")).unwrap();
    let history = unlinked_session.history().unwrap();
    assert!(
        history.iter().any(|message| {
            message.text() == "unlinked work"
                && message.meta().source == Some(radian_core::InputSource::User)
        }),
        "the unlinked session committed its user message, not just any message: {history:?}"
    );

    let lines = session_index_lines(&env, &project);
    assert_eq!(
        lines.len(),
        1,
        "only the linked session is indexed: {lines:?}"
    );
    assert!(lines[0].contains("linked-session"), "{lines:?}");
    assert!(!lines[0].contains("unlinked-session"), "{lines:?}");

    let registry =
        std::fs::read_to_string(env.home.join("memory/projects.jsonl")).unwrap_or_default();
    assert_eq!(
        registry
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "only the linked project is registered: {registry}"
    );
    let projects: Vec<_> = std::fs::read_dir(env.home.join("memory/projects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        projects.len(),
        1,
        "the unlinked run created a project directory: {projects:?}"
    );
}

/// Edge case 7: the model may `read` `SESSIONS.md`; `write` and `patch` to it are refused with
/// the host's reason in both tiers.
#[tokio::test]
async fn the_model_reads_the_session_index_and_cannot_write_it_in_both_tiers() {
    let env = MemoryEnv::sandboxed("mem-sessions-read-only");
    repository(&env.ws);
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;

    let prep = MockServer::start(vec![text_reply("one")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &prep.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--session-id",
                "prep-session",
                "-p",
                "prepare the index",
            ],
            &memory_session_envs(&env),
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let index = session_index_path(&env, &project);
    assert!(
        std::fs::read_to_string(&index)
            .unwrap()
            .contains("prep-session"),
        "the preparation session wrote the index"
    );
    let target = serde_json::to_string(index.to_str().unwrap()).unwrap();

    for (tier, id) in [("workspace", "read-workspace"), ("full", "read-full")] {
        let server = MockServer::start(vec![
            tool_reply("r1", "read", &format!("{{\"path\":{target}}}")),
            tool_reply(
                "w1",
                "write",
                &format!("{{\"path\":{target},\"content\":\"nope\\n\"}}"),
            ),
            tool_reply(
                "p1",
                "patch",
                &format!("{{\"path\":{target},\"old\":\"prep-session\",\"new\":\"nope\"}}"),
            ),
            text_reply("done"),
        ])
        .await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    env.ws.to_str().unwrap(),
                    "--profile",
                    tier,
                    "--yes",
                    "--session-id",
                    id,
                    "-p",
                    "read then write the index",
                ],
                &memory_session_envs(&env),
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{tier}: {stderr}");
        let read = tool_result_for(&server, 1, "r1");
        assert!(
            read.contains("prep-session"),
            "{tier}: the read returned the index: {read}"
        );
        let write = tool_result_for(&server, 2, "w1");
        assert!(
            write.contains("SESSIONS.md is maintained by radian"),
            "{tier}: the write names the host: {write}"
        );
        let patch = tool_result_for(&server, 3, "p1");
        assert!(
            patch.contains("SESSIONS.md is maintained by radian"),
            "{tier}: the patch names the host: {patch}"
        );
        let after = std::fs::read_to_string(&index).unwrap();
        assert!(
            after.contains("prep-session"),
            "{tier}: the existing line is untouched: {after}"
        );
        assert!(
            !after.contains("nope"),
            "{tier}: the refused write changed nothing: {after}"
        );
    }
}

#[tokio::test]
async fn memory_split_and_merge_move_files_and_rename_a_collision() {
    let env = MemoryEnv::new("mem-split");
    let a = env.workspace("a");
    let b = env.workspace("b");
    // A `.git` directory makes each workspace its own project: a real repository, with no
    // `git` process. Without it, a scratch path under this worktree would resolve to the
    // worktree's own project and both `a` and `b` would be one project.
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let (code, _, stderr) = env.run_in(&a, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run_in(&b, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    env.run_in(&a, &["add", "moved-record"]).await;
    env.run_in(&a, &["add", "note"]).await;
    env.run_in(&b, &["add", "note"]).await;
    let project_a = linked_project(&env, &a).await;
    let project_b = linked_project(&env, &b).await;

    // `split` needs confirmation: off a terminal, without --yes it refuses and moves nothing.
    let (code, _, stderr) = env
        .run_in(&a, &["split", &project_a, &project_b, "moved-record"])
        .await;
    assert_eq!(code, 2, "{stderr}");
    let (_, out, _) = env.run_in(&b, &["list"]).await;
    assert!(
        !out.contains("moved-record"),
        "the refused split moved it: {out}"
    );

    let (code, out, stderr) = env
        .run_in(
            &a,
            &["split", "--yes", &project_a, &project_b, "moved-record"],
        )
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("moved moved-record"), "{out}");
    let (_, out, _) = env.run_in(&b, &["list"]).await;
    assert!(out.contains("moved-record"), "the target has it: {out}");
    // The index line moved too.
    let b_index = env
        .home
        .join("memory/projects")
        .join(&project_b)
        .join("MEMORY.md");
    assert!(
        std::fs::read_to_string(&b_index)
            .unwrap()
            .contains("moved-record.md"),
        "the target index has the line"
    );

    // `merge` every file from b into a: `note` exists in both, so b's copy is `note-2`.
    let (code, out, stderr) = env
        .run_in(&b, &["merge", "--yes", &project_a, &project_b])
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("renamed it to note-2"), "{out}");
    assert!(
        !out.contains("it was not moved"),
        "a merge renames its collision instead of reporting a skipped split: {out}"
    );
    let (_, out, _) = env.run_in(&a, &["list"]).await;
    assert!(out.contains("note"), "{out}");
    assert!(out.contains("note-2"), "{out}");
    assert!(out.contains("moved-record"), "{out}");
}

/// `merge` moves record files only: each project keeps its own `SESSIONS.md` (the host's session
/// index), so the source does not lose it and the target's is not replaced.
#[tokio::test]
async fn memory_merge_leaves_the_session_indexes_alone() {
    let env = MemoryEnv::new("mem-merge-sessions");
    let a = env.workspace("a");
    let b = env.workspace("b");
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let (code, _, stderr) = env.run_in(&a, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run_in(&b, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    env.run_in(&b, &["add", "moved-note"]).await;
    let project_a = linked_project(&env, &a).await;
    let project_b = linked_project(&env, &b).await;
    let projects = env.home.join("memory/projects");
    let a_sessions = projects.join(&project_a).join("SESSIONS.md");
    let b_sessions = projects.join(&project_b).join("SESSIONS.md");
    write_index(&a_sessions, "# Sessions\n- target-session\n");
    write_index(&b_sessions, "# Sessions\n- source-session\n");

    let (code, out, stderr) = env
        .run_in(&b, &["merge", "--yes", &project_a, &project_b])
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(out.contains("moved moved-note"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&a_sessions).unwrap(),
        "# Sessions\n- target-session\n",
        "the target's session index was replaced"
    );
    assert_eq!(
        std::fs::read_to_string(&b_sessions).unwrap(),
        "# Sessions\n- source-session\n",
        "the source lost its session index"
    );
    let (_, out, _) = env.run_in(&a, &["list"]).await;
    assert!(out.contains("moved-note"), "the record moved: {out}");
}

/// A merge suffix must be free in both scopes: the rename happens inside the source before the
/// move, so a name the source already has (here `note-2`) is skipped.
#[tokio::test]
async fn memory_merge_picks_a_collision_name_free_in_both_projects() {
    let env = MemoryEnv::new("mem-merge-double-collision");
    let a = env.workspace("a");
    let b = env.workspace("b");
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let (code, _, stderr) = env.run_in(&a, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let (code, _, stderr) = env.run_in(&b, &["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    env.run_in(&a, &["add", "note"]).await;
    env.run_in(&b, &["add", "note"]).await;
    env.run_in(&b, &["add", "note-2"]).await;
    let project_a = linked_project(&env, &a).await;
    let project_b = linked_project(&env, &b).await;

    let (code, out, stderr) = env
        .run_in(&b, &["merge", "--yes", &project_a, &project_b])
        .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        out.contains("conflict: note exists in the target; renamed it to note-3"),
        "the free suffix skips the source's own note-2: {out}"
    );
    let (_, out, _) = env.run_in(&a, &["list"]).await;
    for name in ["note", "note-2", "note-3"] {
        assert!(
            out.lines()
                .any(|line| line.contains(&format!("\t{name}\t"))),
            "the target lists {name}: {out}"
        );
    }
    assert_eq!(
        out.lines().filter(|line| line.contains("\tnote")).count(),
        3,
        "both files moved: {out}"
    );
}

#[tokio::test]
async fn memory_off_refuses_a_model_write_until_memory_on() {
    let env = MemoryEnv::new("mem-off");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let project = linked_project(&env, &env.ws).await;
    let target = env
        .home
        .join("memory/projects")
        .join(&project)
        .join("off-note.md");
    let path = serde_json::to_string(target.to_str().unwrap()).unwrap();
    let write = |call: &str| {
        tool_reply(
            call,
            "write",
            &format!("{{\"path\":{path},\"content\":\"NOTE\\n\"}}"),
        )
    };
    let server = MockServer::start(vec![
        write("w1"),
        text_reply("first"),
        write("w2"),
        text_reply("second"),
    ])
    .await;
    let args = memory_args(&env, &server, "mem-off", None, None);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&refs, &memory_envs(&env)),
        "/memory off\nwrite it\n/memory on\nwrite it\n/exit\n",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    let refused = tool_result_for(&server, 1, "w1");
    assert!(
        refused.contains("memory is off for this session"),
        "the off reason reaches the model: {refused}"
    );
    let allowed = tool_result_for(&server, 3, "w2");
    assert!(
        !allowed.contains("memory is off for this session"),
        "after /memory on the write runs: {allowed}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "NOTE\n",
        "only the on write reached the disk"
    );
}

#[tokio::test]
async fn a_session_resumed_after_memory_off_has_no_memory_section() {
    let env = MemoryEnv::new("mem-off-resume");
    let (_, _, _) = env.run(&["project", "new"]).await;
    let project = linked_project(&env, &env.ws).await;
    let index = project_index_path(&env, &project);
    write_index(&index, "- [r](r.md) — REMEMBER-ME\n");

    let first = MockServer::start(vec![text_reply("off")]).await;
    let mut args = memory_args(&env, &first, "mem-off-resume", None, None);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(&refs, &memory_envs(&env)),
        "/memory off\nhello\n/exit\n",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");

    // Resume: the recorded switch means this process starts without the section.
    let second = MockServer::start(vec![text_reply("resumed")]).await;
    args = memory_args(&env, &second, "unused", Some("hi"), None);
    let at = args.iter().position(|arg| arg == "--session-id").unwrap();
    args[at] = "--resume".to_owned();
    args[at + 1] = "mem-off-resume".to_owned();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, stderr) = feed_and_wait(spawn_radian(&refs, &memory_envs(&env)), "").await;
    assert_eq!(code, 0, "{stderr}");
    let system = system_prompt(&second, 0);
    assert!(
        !system.contains("REMEMBER-ME") && !system.contains("Memory indexes"),
        "a resumed off session injects no memory section: {system}"
    );
}

// ---------- memory index injection (ADR 0021 D6) ----------

/// The `radian exec` arguments for one run against `server`.
fn memory_args(
    env: &MemoryEnv,
    server: &MockServer,
    session: &str,
    prompt: Option<&str>,
    config: Option<&Path>,
) -> Vec<String> {
    let mut args = vec![
        "--model".to_owned(),
        "m".to_owned(),
        "--base-url".to_owned(),
        server.base_url.clone(),
        "--workspace".to_owned(),
        env.ws.to_string_lossy().into_owned(),
        "--session-id".to_owned(),
        session.to_owned(),
    ];
    if let Some(config) = config {
        args.push("--config".to_owned());
        args.push(config.to_string_lossy().into_owned());
    }
    if let Some(prompt) = prompt {
        args.push("-p".to_owned());
        args.push(prompt.to_owned());
    }
    args
}

fn memory_envs(env: &MemoryEnv) -> [(&'static str, &str); 2] {
    [
        ("RADIAN_HOME", env.home.to_str().unwrap()),
        ("RADIAN_SESSION_DIR", env.sessions.to_str().unwrap()),
    ]
}

async fn run_memory_session(
    env: &MemoryEnv,
    server: &MockServer,
    session: &str,
    prompt: &str,
    config: Option<&Path>,
) -> (i32, String, String) {
    let args = memory_args(env, server, session, Some(prompt), config);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    feed_and_wait(spawn_radian(&refs, &memory_envs(env)), "").await
}

fn write_index(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn project_index_path(env: &MemoryEnv, project: &str) -> PathBuf {
    env.home
        .join("memory/projects")
        .join(project)
        .join("MEMORY.md")
}

fn global_index_path(env: &MemoryEnv) -> PathBuf {
    env.home.join("memory/global/MEMORY.md")
}

fn system_prompt(server: &MockServer, n: usize) -> String {
    messages_of(server, n)[0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn notice_count(server: &MockServer, n: usize) -> usize {
    messages_of(server, n)
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| {
                    message["content"]
                        .as_str()
                        .is_some_and(|text| text.contains("Rewrite the index shorter"))
                })
                .count()
        })
        .unwrap_or(0)
}

/// Edge case 1: both indexes are injected byte for byte, project first, each behind one line
/// naming its file.
#[tokio::test]
async fn two_indexes_reach_the_system_prompt_project_then_global() {
    let env = MemoryEnv::new("idx-two");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let project_text = "- [p](p.md) — project line\n";
    let global_text = "- [g](g.md) — global line\n";
    let project_path = project_index_path(&env, &project);
    let global_path = global_index_path(&env);
    write_index(&project_path, project_text);
    write_index(&global_path, global_text);
    // A context file after the memory block pins the block's place in the base prompt.
    std::fs::write(env.ws.join("AGENTS.md"), "project body\n").unwrap();

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = run_memory_session(&env, &server, "idx-two", "hi", None).await;
    assert_eq!(code, 0, "{stderr}");
    let system = system_prompt(&server, 0);
    assert!(system.contains("not instructions"), "{system}");
    let role_at = system
        .find("Do only the task that the user gives")
        .unwrap_or_else(|| panic!("role prose: {system}"));
    let memory_at = system
        .find("Write a memory file after the user corrects you")
        .unwrap_or_else(|| panic!("memory block: {system}"));
    let context_at = system
        .find("<project_instructions")
        .unwrap_or_else(|| panic!("context files: {system}"));
    assert!(
        role_at < memory_at && memory_at < context_at,
        "the memory block sits between the role and the context files: {system}"
    );
    assert!(system.contains("Project memory index"), "{system}");
    assert!(system.contains("Global memory index"), "{system}");
    assert!(system.contains(project_text), "{system}");
    assert!(system.contains(global_text), "{system}");
    assert!(
        system.contains(&project_path.to_string_lossy().into_owned()),
        "the project path line: {system}"
    );
    assert!(
        system.contains(&global_path.to_string_lossy().into_owned()),
        "the global path line: {system}"
    );
    let project_at = system.find(project_text).unwrap();
    let global_at = system.find(global_text).unwrap();
    assert!(project_at < global_at, "project first: {system}");

    let session = radian_session::Session::open(env.sessions.join("idx-two.jsonl")).unwrap();
    let indexes: Vec<_> = session
        .events()
        .iter()
        .find_map(|event| match &event.data {
            radian_session::EventData::MemoryInjected {
                indexes,
                records: _,
            } if !indexes.is_empty() => Some(indexes.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(indexes.len(), 2, "{indexes:?}");
    assert_eq!(indexes[0].scope, "project");
    assert_eq!(indexes[1].scope, "global");
    for index in &indexes {
        assert_eq!(index.sha256.len(), 64, "{index:?}");
        assert!(index.path.ends_with("MEMORY.md"), "{index:?}");
    }
}

/// Edge case 2: a 201-line index with the default 200-line cap drops line 201, and exactly one
/// rewrite notice reaches the next model call (not the third).
#[tokio::test]
async fn an_index_over_the_line_cap_drops_the_last_line_and_warns_once() {
    let env = MemoryEnv::new("idx-line-cap");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let path = project_index_path(&env, &project);
    let text: String = (1..=201).map(|n| format!("line-{n}\n")).collect();
    write_index(&path, &text);

    let server = MockServer::start(vec![
        tool_reply("c1", "read", r#"{"path":"missing-one.txt"}"#),
        tool_reply("c2", "read", r#"{"path":"missing-two.txt"}"#),
        text_reply("done"),
    ])
    .await;
    let (code, _, stderr) = run_memory_session(&env, &server, "idx-line-cap", "hi", None).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 3, "{}", server.requests().len());

    let system = system_prompt(&server, 0);
    assert!(system.contains("line-1\n"), "{system}");
    assert!(system.contains("line-200\n"), "{system}");
    assert!(!system.contains("line-201"), "the cap drops it: {system}");

    assert_eq!(notice_count(&server, 1), 1, "the next call sees one notice");
    let notices: Vec<String> = messages_of(&server, 1)
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .filter(|text| text.contains("Rewrite the index shorter"))
        .collect();
    assert!(notices[0].contains("project"), "{}", notices[0]);
    assert!(
        notices[0].contains(&path.to_string_lossy().into_owned()),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains("1 line was dropped"), "{}", notices[0]);
    assert_eq!(
        notice_count(&server, 2),
        1,
        "a third call gets no second notice"
    );
}

/// Edge case 3: an index under the line cap but over the byte cap is cut at the last whole line
/// within the byte cap, and the notice is sent.
#[tokio::test]
async fn an_index_over_the_byte_cap_is_cut_at_a_line_boundary() {
    let env = MemoryEnv::new("idx-byte-cap");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    write_index(&project_index_path(&env, &project), "aaa\nbbb\nccc\n");
    let config = write_file(
        &env.dir,
        "config.toml",
        "schema_version = 1\n[memory]\nindex_max_bytes = 8\n",
    );

    let server = MockServer::start(vec![
        tool_reply("c1", "read", r#"{"path":"missing.txt"}"#),
        text_reply("done"),
    ])
    .await;
    let (code, _, stderr) =
        run_memory_session(&env, &server, "idx-byte-cap", "hi", Some(&config)).await;
    assert_eq!(code, 0, "{stderr}");
    let system = system_prompt(&server, 0);
    assert!(system.contains("aaa\n"), "{system}");
    assert!(system.contains("bbb\n"), "{system}");
    assert!(
        !system.contains("ccc"),
        "the third line does not fit: {system}"
    );
    assert_eq!(notice_count(&server, 1), 1);
}

/// Edge case 4: a workspace that maps to no project injects only the global index and one line
/// saying the project scope has no index. A read never creates a project (ADR 0021 D2).
#[tokio::test]
async fn an_unlinked_workspace_injects_only_the_global_index() {
    let env = MemoryEnv::new("idx-unlinked");
    write_index(&global_index_path(&env), "global only\n");

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = run_memory_session(&env, &server, "idx-unlinked", "hi", None).await;
    assert_eq!(code, 0, "{stderr}");
    let system = system_prompt(&server, 0);
    assert!(system.contains("global only"), "{system}");
    assert!(
        system.to_lowercase().contains("no project memory index"),
        "{system}"
    );
    assert!(
        !env.home.join("memory/projects").exists(),
        "the injection created a project directory"
    );
}

/// Edge case 5: `[memory] enabled = false` injects nothing, sends no notice and writes no
/// `MemoryInjected`.
#[tokio::test]
async fn memory_disabled_injects_nothing_and_writes_no_record() {
    let env = MemoryEnv::new("idx-disabled");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let text: String = (1..=201).map(|n| format!("line-{n}\n")).collect();
    write_index(&project_index_path(&env, &project), &text);
    write_index(&global_index_path(&env), "global note\n");
    let config = write_file(
        &env.dir,
        "config.toml",
        "schema_version = 1\n[memory]\nenabled = false\n",
    );

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) =
        run_memory_session(&env, &server, "idx-disabled", "hi", Some(&config)).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 1);
    let system = system_prompt(&server, 0);
    assert!(!system.contains("Memory indexes"), "{system}");
    for sentence in radian_host::prompt::MEMORY_RULES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        assert!(
            !system.contains(sentence),
            "memory is disabled, but the prompt carries {sentence:?}: {system}"
        );
    }
    assert!(!system.contains("line-1\n"), "{system}");
    assert!(!system.contains("global note"), "{system}");
    assert_eq!(notice_count(&server, 0), 0);
    let raw = std::fs::read_to_string(env.sessions.join("idx-disabled.jsonl")).unwrap();
    assert!(!raw.contains("MemoryInjected"), "{raw}");
}

/// Edge case 6: the section is identical on the second call after the index changes on disk; a
/// resumed session reads the file again.
#[tokio::test]
async fn the_section_freezes_per_process_and_a_resume_reads_again() {
    let env = MemoryEnv::new("idx-freeze");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    let path = project_index_path(&env, &project);
    write_index(&path, "before\n");

    let server = MockServer::start(vec![text_reply("one"), text_reply("two")]).await;
    let args = memory_args(&env, &server, "idx-freeze", None, None);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = spawn_radian(&refs, &memory_envs(&env));
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"one\n").await.unwrap();
    stdin.flush().await.unwrap();
    wait_until("the first turn is finished", || {
        journal_turns(&env.sessions) >= 1
    })
    .await;
    // The file changes between the two turns of one process.
    write_index(&path, "after\n");
    stdin.write_all(b"two\n").await.unwrap();
    stdin.flush().await.unwrap();
    wait_until("the second turn is finished", || {
        journal_turns(&env.sessions) >= 2
    })
    .await;
    drop(stdin);
    let output = timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("the CLI exits")
        .expect("wait");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(server.requests().len(), 2);
    let first = system_prompt(&server, 0);
    assert!(first.contains("before\n"), "{first}");
    assert!(!first.contains("after\n"), "{first}");
    assert_eq!(system_prompt(&server, 1), first, "the section is frozen");

    // A resume is a new process: it reads the file again.
    let server = MockServer::start(vec![text_reply("three")]).await;
    let mut args = memory_args(&env, &server, "unused", Some("three"), None);
    // Replace `--session-id idx` with `--resume idx-freeze`.
    let at = args.iter().position(|arg| arg == "--session-id").unwrap();
    args[at] = "--resume".to_owned();
    args[at + 1] = "idx-freeze".to_owned();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, _, stderr) = feed_and_wait(spawn_radian(&refs, &memory_envs(&env)), "").await;
    assert_eq!(code, 0, "{stderr}");
    let resumed = system_prompt(&server, 0);
    assert!(resumed.contains("after\n"), "{resumed}");
    assert!(!resumed.contains("before\n"), "{resumed}");
}

/// Edge case 7: a retired `[memory.injection]` key gets one migration warning naming it, and the
/// session starts.
#[tokio::test]
async fn a_retired_memory_injection_key_warns_and_the_session_starts() {
    let env = MemoryEnv::new("idx-retired");
    let config = write_file(
        &env.dir,
        "config.toml",
        "schema_version = 1\n[memory.injection]\nmax_tokens = 1000\n",
    );

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) =
        run_memory_session(&env, &server, "idx-retired", "hi", Some(&config)).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok"), "{stdout}");
    assert!(
        stderr.contains("memory.injection.max_tokens"),
        "the migration warning names the key: {stderr}"
    );
    assert!(
        stderr.contains("index_max_lines"),
        "the warning names the replacement: {stderr}"
    );
}

/// Edge case 9: a model call to `memory_search` is an unknown tool, like `memory_save`, and the
/// turn still succeeds.
#[tokio::test]
async fn a_model_call_to_memory_save_or_memory_search_is_an_unknown_tool_and_the_turn_succeeds() {
    let env = MemoryEnv::new("mem-no-write-tool");
    let server = MockServer::start(vec![
        tool_reply("call_1", "memory_save", r#"{"content":"agent note"}"#),
        tool_reply("call_2", "memory_search", r#"{"query":"agent note"}"#),
        text_reply("done"),
    ])
    .await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "-p",
                "remember this",
            ],
            &[
                ("RADIAN_HOME", env.home.to_str().unwrap()),
                ("RADIAN_SESSION_DIR", env.sessions.to_str().unwrap()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(server.requests().len(), 3);
    assert!(
        tool_result_for(&server, 1, "call_1").contains("no tool named `memory_save`"),
        "the model is told the save tool does not exist: {}",
        tool_result_for(&server, 1, "call_1")
    );
    assert!(
        tool_result_for(&server, 2, "call_2").contains("no tool named `memory_search`"),
        "the model is told the search tool does not exist: {}",
        tool_result_for(&server, 2, "call_2")
    );
    let (code, stdout, stderr) = env.run(&["list", "--json"]).await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout.trim(),
        "[]",
        "the refused calls wrote nothing: {stdout}"
    );
}

/// Edge case 10: an index with an invalid byte sequence contributes one line saying it could not
/// be read; the session starts and a warning names the file.
#[tokio::test]
async fn an_invalid_utf8_index_warns_and_the_session_starts() {
    let env = MemoryEnv::new("idx-bad-utf8");
    let (code, _, stderr) = env.run(&["project", "new"]).await;
    assert_eq!(code, 0, "{stderr}");
    let project = linked_project(&env, &env.ws).await;
    write_index(&project_index_path(&env, &project), "project note\n");
    let global = global_index_path(&env);
    std::fs::create_dir_all(global.parent().unwrap()).unwrap();
    std::fs::write(&global, [0xff, 0xfe, 0xfd]).unwrap();

    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) =
        run_memory_session(&env, &server, "idx-bad-utf8", "hi", None).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok"), "{stdout}");
    let system = system_prompt(&server, 0);
    assert!(system.contains("project note"), "{system}");
    assert!(
        system.contains("The global memory index at") && system.contains("could not be read"),
        "{system}"
    );
    assert!(
        stderr.contains(&global.to_string_lossy().into_owned()),
        "the warning names the file: {stderr}"
    );
}

// ---------- memory write guards (ADR 0021 invariant 4) ----------

/// The `radian exec` arguments for one memory session with the MCP fixture attached.
fn memory_mcp_args(
    env: &MemoryEnv,
    server: &MockServer,
    session: &str,
    prompt: &str,
    mcp: &Path,
) -> Vec<String> {
    let mut args = memory_args(env, server, session, Some(prompt), None);
    args.push("--mcp-config".to_owned());
    args.push(mcp.to_string_lossy().into_owned());
    args
}

async fn run_memory_mcp_session(
    env: &MemoryEnv,
    server: &MockServer,
    session: &str,
    prompt: &str,
    mcp: &Path,
) -> (i32, String, String) {
    let args = memory_mcp_args(env, server, session, prompt, mcp);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    feed_and_wait(spawn_radian(&refs, &memory_envs(env)), "").await
}

/// Edge case 1: one MCP result makes every later memory write be refused; the same write
/// before the MCP call succeeds. The refused call touches no byte.
#[tokio::test]
async fn a_memory_write_is_refused_after_an_mcp_result_and_allowed_before() {
    let env = MemoryEnv::sandboxed("guard-input");
    let log = env.dir.join("fixture.log");
    let mcp = mcp_config(&env.dir, &log);
    let file = env.home.join("memory/global/x.md");
    let write = |content: &str| serde_json::json!({"path": file, "content": content}).to_string();

    let server = MockServer::start(vec![
        tool_reply("c1", "write", &write("before\n")),
        tool_reply("c2", "fx__echo", ECHO_CALL),
        tool_reply("c3", "write", &write("after\n")),
        text_reply("done"),
    ])
    .await;
    let (code, stdout, stderr) =
        run_memory_mcp_session(&env, &server, "guard-input", "remember", &mcp).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");

    let first = tool_result_for(&server, 1, "c1");
    assert!(
        !first.contains("refused"),
        "the write before MCP runs: {first}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "before\n",
        "the refused second write changed nothing"
    );
    let refused = tool_result_for(&server, 3, "c3");
    assert!(
        refused.contains(radian_host::memory::INPUT_GUARD),
        "the input guard names itself: {refused}"
    );
}

/// Edge case 2: a workspace path is not a memory path, so an MCP result does not affect it.
#[tokio::test]
async fn a_workspace_write_after_an_mcp_result_is_unaffected() {
    let env = MemoryEnv::sandboxed("guard-workspace");
    let log = env.dir.join("fixture.log");
    let mcp = mcp_config(&env.dir, &log);
    let file = env.ws.join("after-mcp.txt");
    let write = serde_json::json!({"path": file, "content": "WORKSPACE-AFTER-MCP\n"}).to_string();

    let server = MockServer::start(vec![
        tool_reply("c1", "fx__echo", ECHO_CALL),
        tool_reply("c2", "write", &write),
        text_reply("done"),
    ])
    .await;
    let (code, stdout, stderr) =
        run_memory_mcp_session(&env, &server, "guard-workspace", "go", &mcp).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "WORKSPACE-AFTER-MCP\n",
        "a workspace write keeps working"
    );
    let result = tool_result_for(&server, 2, "c2");
    assert!(!result.contains("refused"), "{result}");
}

/// Edge case 3: a resumed session whose committed journal holds an MCP tool result refuses
/// its first memory write, even though this process never called an MCP tool.
#[tokio::test]
async fn a_resumed_session_counts_an_mcp_result_and_refuses_the_first_memory_write() {
    let env = MemoryEnv::sandboxed("guard-resume");
    let log = env.dir.join("fixture.log");
    let mcp = mcp_config(&env.dir, &log);
    let server = MockServer::start(vec![
        tool_reply("c1", "fx__echo", ECHO_CALL),
        text_reply("done"),
    ])
    .await;
    let (code, _, stderr) = run_memory_mcp_session(&env, &server, "guard-resume", "go", &mcp).await;
    assert_eq!(code, 0, "{stderr}");
    // The first process settled the routed `fx__echo` call; the journal is the host's record
    // of it. Assert that specific result, not merely that some call settled.
    let session = radian_session::Session::open(env.sessions.join("guard-resume.jsonl")).unwrap();
    let settled = session.events().iter().find_map(|event| match &event.data {
        radian_session::EventData::ToolExecutionSettled { call_id, result } if call_id == "c1" => {
            Some(result)
        }
        _ => None,
    });
    let settled = settled.expect("the journal holds the fx__echo result");
    assert!(!settled.is_error, "the fixture's MCP call succeeded");
    assert_ne!(
        settled.error_code.as_deref(),
        Some("unknown_tool"),
        "the journaled call reached the fixture server"
    );

    let file = env.home.join("memory/global/x.md");
    let write = serde_json::json!({"path": &file, "content": "after-resume\n"}).to_string();
    let server =
        MockServer::start(vec![tool_reply("w1", "write", &write), text_reply("done")]).await;
    // The resumed process still configures the same fixture server, so `fx__echo` is a name it
    // routes; the guard must judge the journal against that list, not against `__` alone.
    let mut args = memory_mcp_args(&env, &server, "unused", "write", &mcp);
    let at = args.iter().position(|arg| arg == "--session-id").unwrap();
    args[at] = "--resume".to_owned();
    args[at + 1] = "guard-resume".to_owned();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = feed_and_wait(spawn_radian(&refs, &memory_envs(&env)), "").await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert!(!file.exists(), "the refused write created no file");
    let refused = tool_result_for(&server, 1, "w1");
    assert!(
        refused.contains(radian_host::memory::INPUT_GUARD),
        "the resumed process counts the journal's MCP result: {refused}"
    );
}

/// Edge case 4: content with a secret prefix is refused, the message names the prefix and
/// never the value; the same prefix inside a word is accepted by the heuristic.
#[tokio::test]
async fn a_secret_in_a_memory_write_is_refused_and_a_prefix_inside_a_word_is_allowed() {
    let env = MemoryEnv::sandboxed("guard-secret");
    let bad = env.home.join("memory/global/bad.md");
    let good = env.home.join("memory/global/good.md");
    let write = |path: &Path, content: &str| {
        serde_json::json!({"path": path, "content": content}).to_string()
    };

    let server = MockServer::start(vec![
        tool_reply("c1", "write", &write(&bad, "sk-abcdefghijklmnopqrst\n")),
        tool_reply(
            "c2",
            "write",
            &write(&good, "the musk-abcdefghijklmnopqrst note\n"),
        ),
        text_reply("done"),
    ])
    .await;
    let (code, stdout, stderr) =
        run_memory_session(&env, &server, "guard-secret", "remember", None).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert!(!bad.exists(), "the refused write created no file");
    assert_eq!(
        std::fs::read_to_string(&good).unwrap(),
        "the musk-abcdefghijklmnopqrst note\n",
        "a prefix inside a word is not a secret"
    );
    let refused = tool_result_for(&server, 1, "c1");
    assert!(
        refused.contains("memory write refused: the content looks like a secret (sk-)"),
        "the refusal names the prefix: {refused}"
    );
    assert!(
        !refused.contains("abcdefghijklmnopqrst"),
        "the refusal never repeats the value: {refused}"
    );
}

/// Edge case 5: a patch that would insert a secret into an existing memory file is refused
/// before the file is touched.
#[tokio::test]
async fn a_patch_that_would_insert_a_secret_is_refused_and_the_file_is_unchanged() {
    let env = MemoryEnv::sandboxed("guard-patch");
    let file = env.home.join("memory/global/note.md");
    let before = "---\nname: note\ndescription: a note\ntype: project\nsource: model\n\
                  created: 2026-01-01T00:00:00Z\nmodified: 2026-01-01T00:00:00Z\n---\nsafe\n";
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, before).unwrap();

    let patch = serde_json::json!({
        "path": &file,
        "old": "safe",
        "new": "sk-abcdefghijklmnopqrst",
    })
    .to_string();
    let server =
        MockServer::start(vec![tool_reply("c1", "patch", &patch), text_reply("done")]).await;
    let (code, stdout, stderr) =
        run_memory_session(&env, &server, "guard-patch", "remember", None).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "the refused patch left every byte alone"
    );
    let refused = tool_result_for(&server, 1, "c1");
    assert!(
        refused.contains("memory write refused: the content looks like a secret (sk-)"),
        "the patch is judged on the content it would produce: {refused}"
    );
    assert!(
        !refused.contains("abcdefghijklmnopqrst"),
        "the refusal never repeats the value: {refused}"
    );
}

/// A patch needle is located by the tool's normalising layers (here the LF view matching a
/// CRLF needle), so the guard must judge the content the edit would actually write. The
/// inserted text alone is clean; the result is not.
#[tokio::test]
async fn a_patch_matched_through_a_normalising_locator_still_catches_a_secret() {
    let env = MemoryEnv::sandboxed("guard-patch-crlf");
    let file = env.home.join("memory/global/crlf.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let before = "sk-\nX";
    std::fs::write(&file, before).unwrap();

    let patch = serde_json::json!({
        "path": &file,
        "old": "\r\nX",
        "new": "abcdefghijklmnopqrst",
    })
    .to_string();
    let server =
        MockServer::start(vec![tool_reply("c1", "patch", &patch), text_reply("done")]).await;
    let (code, stdout, stderr) =
        run_memory_session(&env, &server, "guard-patch-crlf", "remember", None).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("done"), "{stdout}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        before,
        "the refused patch left every byte alone"
    );
    let refused = tool_result_for(&server, 1, "c1");
    assert!(
        refused.contains("memory write refused: the content looks like a secret (sk-)"),
        "the result of the normalising match is judged: {refused}"
    );
    assert!(
        !refused.contains("abcdefghijklmnopqrst"),
        "the refusal never repeats the value: {refused}"
    );
}

#[tokio::test]
async fn an_interactive_compact_is_a_command_and_compactfoo_is_a_message() {
    let server = MockServer::start(vec![text_reply("REPLY-TO-COMPACTFOO")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(&["--model", "m", "--base-url", &server.base_url], &[]),
        "/compact\n/compactfoo\n/compact note\n/exit\n",
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(stdout, "REPLY-TO-COMPACTFOO\n");
    // Both `/compact` and `/compact note` report a result; neither reaches the model.
    assert_eq!(stderr.matches("nothing to compact").count(), 2, "{stderr}");
    assert_eq!(server.requests().len(), 1, "only `/compactfoo` was a turn");
    assert_eq!(last_user_text(&messages_of(&server, 0)), "/compactfoo");
}

// ---------- the configuration file (ADR 0008) ----------

/// A `--config` file supplies the model (so `--model` is not needed), and a `--model` on the
/// command line overrides it. `RADIAN_CONFIG` is the fallback when `--config` is absent.
#[tokio::test]
async fn a_config_file_supplies_the_model_and_the_cli_overrides_it() {
    let dir = scratch("config-model");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[model]\nmodel = \"config-model\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("A"), text_reply("B"), text_reply("C")]).await;
    let sessions = dir.join("sessions");

    // No `--model`: the config supplies it.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("[config]"),
        "the startup line names the file: {stderr}"
    );
    assert_eq!(server.requests()[0].json()["model"], "config-model");

    // `--model` wins over the file.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--model",
                "cli-model",
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests()[1].json()["model"], "cli-model");

    // `RADIAN_CONFIG` is the fallback.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[("RADIAN_CONFIG", config.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests()[2].json()["model"], "config-model");
}

/// A `schema_version` below the supported one is refused at startup, before any session file
/// exists (ADR 0008 §4: no v0→v1 path, and the file is never rewritten).
#[tokio::test]
async fn a_lower_schema_version_refuses_to_start_without_creating_a_session() {
    let dir = scratch("config-lower");
    let config = dir.join("config.toml");
    std::fs::write(&config, "schema_version = 0\n[model]\nmodel = \"m\"\n").unwrap();
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("schema_version"), "{stderr}");
    assert!(
        jsonl_snapshot(&sessions).is_empty(),
        "no session file may be created: {:?}",
        dir_names(&sessions)
    );
    assert!(server.requests().is_empty(), "no request is sent");
}

/// A plaintext secret in the config is refused, and the error never echoes the value.
#[tokio::test]
async fn a_plaintext_secret_in_the_config_is_refused_without_echoing_it() {
    let dir = scratch("config-secret");
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[model]\nmodel = \"m\"\napi_key = \"sk-live-abcdef123456\"\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("never")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--config",
                config.to_str().unwrap(),
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("api_key"), "{stderr}");
    assert!(
        !stderr.contains("sk-live"),
        "the value must not be echoed: {stderr}"
    );
}

/// The config reader takes `schema_version = 0` from **the harness's temp `RADIAN_HOME`** and
/// names that path: the reader and the harness agree on the root, so a test config lives in
/// the temp directory and never in the user's real one.
#[tokio::test]
async fn a_failing_config_in_the_temp_radian_home_is_read_and_named() {
    let dir = scratch("config-rh");
    let config_dir = dir.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(&config, "schema_version = 0\n").unwrap();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "-p", "hi"],
            &[("RADIAN_HOME", dir.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("older than the supported"), "{stderr}");
    assert!(
        stderr.contains(&config.display().to_string()),
        "the refusal names the temp config: {stderr}"
    );
    // The refusal happens before any API key is read.
    assert!(!stderr.contains("no API key"), "{stderr}");
}

/// The harness must never read the user's real `~/.radian/config/config.toml`. `spawn_radian`
/// pins the whole root (`RADIAN_HOME`) and drops `RADIAN_CONFIG`, so a `HOME` whose
/// `.radian/config/config.toml` would refuse startup is **not** consulted. ADR 0008 §2: with
/// no config in the pinned root the run creates the commented default there and names it —
/// never the outside file.
#[tokio::test]
async fn the_harness_never_reads_a_config_outside_its_temp_root() {
    let dir = scratch("config-fake-home");
    let config_dir = dir.join(".radian").join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), "schema_version = 0\n").unwrap();
    let home = dir.join("rh");

    // A local mock keeps the run past the API-key check, so the `[config]` startup line is
    // reached.
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[
                ("HOME", dir.to_str().unwrap()),
                ("RADIAN_HOME", home.to_str().unwrap()),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        !stderr.contains("older than the supported"),
        "a config outside the harness root must not be read: {stderr}"
    );
    let expected = home.join("config/config.toml");
    assert!(
        stderr.contains(&format!(
            "created a default config at {}",
            expected.display()
        )),
        "the default is created inside the harness root: {stderr}"
    );
    assert!(
        stderr.contains(&format!("[config] {}", expected.display())),
        "the run reads the created file, not the outside one: {stderr}"
    );
    assert_eq!(server.requests().len(), 1, "the run reached the model");
}

// ---------- dogfooding D21: a closed stdout is not a panic ----------

/// A stdout whose reader is already gone, so the child's first write is `EPIPE` no matter how
/// much it writes: the read end is closed before the child starts, which does not depend on
/// the pipe buffer size. `SIGPIPE` stays ignored (Rust's default), so the write returns
/// `BrokenPipe` instead of killing the process.
#[cfg(unix)]
fn closed_stdout() -> Stdio {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    // SAFETY: a fresh pipe; both fds are owned here.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: closing a fd we own.
    unsafe { libc::close(fds[0]) };
    // SAFETY: `fds[1]` is an owned write fd, moved into the child's stdout.
    unsafe { Stdio::from_raw_fd(fds[1]) }
}

/// D21: every "print and exit" command must treat a closed stdout as a normal stop: exit 0,
/// no panic on stderr.
#[cfg(unix)]
#[test]
fn a_closed_stdout_is_not_a_panic_for_every_output_command() {
    let dir = scratch("sigpipe");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    write_session(
        &sessions,
        "s1",
        &canonical(&ws),
        "hello sigpipe",
        None,
        None,
    );

    // One memory record, so `radian memory list` has a line to write.
    let home = scratch("sigpipe-home");
    let add = std::process::Command::new(common::radian_bin())
        .args(["memory", "add", "--global", "memory-note"])
        .env("RADIAN_HOME", home.as_os_str())
        .env("HOME", home.as_os_str())
        .env("EDITOR", "true")
        .env("VISUAL", "true")
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run radian memory add");
    assert!(
        add.status.success(),
        "radian memory add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let sessions_arg = sessions.to_str().unwrap();
    let ws_arg = ws.to_str().unwrap();
    let home_arg = home.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--list",
        ],
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--search",
            "hello",
        ],
        vec![
            "--model",
            "m",
            "--session-dir",
            sessions_arg,
            "--workspace",
            ws_arg,
            "--export",
            "s1",
        ],
        vec!["memory", "list", "--global"],
        vec!["trust", "status"],
    ];
    for args in cases {
        let mut child = std::process::Command::new(common::radian_bin())
            .arg("exec")
            .args(&args)
            .env("RADIAN_HOME", home_arg)
            .env("HOME", home_arg)
            .env("RADIAN_SESSION_DIR", sessions_arg)
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .stdin(Stdio::null())
            .stdout(closed_stdout())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn agent");
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        let status = child.wait().expect("wait");
        assert_eq!(status.code(), Some(0), "args {args:?}: stderr {stderr}");
        assert!(!stderr.contains("panicked"), "args {args:?}: {stderr}");
    }
}

/// D22 wording: `--list`/`--search` no longer silently ignore `--session-id`; clap refuses the
/// combination with a usage error (exit 2).
#[tokio::test]
async fn list_and_search_refuse_session_id() {
    for args in [
        vec!["--list", "--session-id", "x"],
        vec!["--search", "x", "--session-id", "x"],
    ] {
        let (code, _, stderr) = feed_and_wait(spawn_radian(&args, &[]), "").await;
        assert_eq!(code, 2, "args {args:?}: {stderr}");
        assert!(stderr.contains("--session-id"), "args {args:?}: {stderr}");
    }
}

// ---------- session fork (ADR 0022) ----------

/// Writes a parent with one finished turn: `PARENT-TEXT` as the first user message. Returns
/// the `TurnFinished` seq (the fork boundary the CLI should default to).
async fn write_turn_session(sessions: &Path, id: &str, workspace: &str) -> u64 {
    use radian_core::{JournalRecord, Message, TranscriptJournal};
    let path = sessions.join(format!("{id}.jsonl"));
    let journal = radian_session::SessionJournal::create(
        &path,
        id,
        radian_session::SessionCreatedData::new(workspace, json!({})),
    )
    .unwrap();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnStarted {
            turn_id: "t-1".into(),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("PARENT-TEXT"),
        })
        .await
        .unwrap();
    let seq = journal.next_seq();
    journal
        .append_runtime_event(radian_session::RuntimeEvent::TurnFinished {
            turn_id: "t-1".into(),
            outcome: radian_session::TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        })
        .await
        .unwrap();
    drop(journal);
    seq
}

/// `radian exec --fork` creates a child with the parent's history, records the lineage in both
/// files, and refuses a parent another process holds.
#[tokio::test]
async fn fork_creates_a_child_and_refuses_a_locked_parent() {
    let dir = scratch("fork-cli");
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let workspace = canonical(&ws);
    let boundary = write_turn_session(&sessions, "parent", &workspace).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                // No --model: --fork is a session command.
                "--fork",
                "parent",
                "--session-id",
                "child",
                "--session-dir",
                sessions.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains(&format!("forked parent at seq {boundary} into child")),
        "{stdout}"
    );
    assert!(stdout.contains("--resume child"), "{stdout}");

    // The child's initial history is the parent's one turn.
    let child = std::fs::read_to_string(sessions.join("child.jsonl")).unwrap();
    assert!(
        child.contains(&format!(
            "\"parent\":{{\"session_id\":\"parent\",\"seq\":{boundary},\"relation\":\"fork\"}}"
        )),
        "{child}"
    );
    assert!(child.contains("PARENT-TEXT"), "{child}");

    // The parent gained exactly one SessionForked with the same seq.
    let parent = std::fs::read_to_string(sessions.join("parent.jsonl")).unwrap();
    assert_eq!(parent.matches("\"event\":\"SessionForked\"").count(), 1);
    assert!(
        parent.contains(&format!(
            "\"child_session_id\":\"child\",\"parent_seq\":{boundary}"
        )),
        "{parent}"
    );

    // A parent another process is writing must be refused with exit 1, not waited for.
    let held =
        radian_session::SessionJournal::resume(sessions.join("parent.jsonl"), "parent").unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--fork",
                "parent",
                "--session-id",
                "child-2",
                "--session-dir",
                sessions.to_str().unwrap(),
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("being written by another process"),
        "{stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert!(!sessions.join("child-2.jsonl").exists());
    drop(held);
}

// ---------- Pi configuration migration (Phase 5) ----------

/// A Pi agent directory: an endpoint, a model, a `${OPENAI_API_KEY}` reference, one plain
/// header, one interpolated header, a theme, a compaction preference and a keybinding. This
/// is a fixture in a temporary directory — the real `~/.pi` is never read.
fn write_pi_fixture(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        r#"{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5",
  "theme": "dark",
  "compaction": { "enabled": false },
  "defaultThinkingLevel": "high"
}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        r#"{
  "providers": {
    "openai": {
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-responses",
      "apiKey": "${OPENAI_API_KEY}",
      "headers": { "x-tenant": "team", "x-trace": "Bearer ${TRACE_TOKEN}" },
      "models": [{ "id": "gpt-5" }]
    }
  }
}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("keybindings.json"),
        r#"{ "tui.input.submit": "ctrl+s" }"#,
    )
    .unwrap();
}

/// Acceptance 1: a typical Pi directory drafts a configuration the Radian reader accepts, with
/// the right values, and `api_key` is an environment reference. Acceptance 2 (first half): a
/// literal `apiKey` becomes a suggested reference and its value is in neither stream.
#[tokio::test]
async fn config_import_pi_drafts_a_valid_configuration_and_never_shows_a_literal_key() {
    let dir = scratch("import-pi");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &stdout)
        .expect("the draft must be a valid Radian configuration");
    assert_eq!(
        config.model.base_url.as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(config.model.model.as_deref(), Some("gpt-5"));
    // ADR 0026 D2: no `model.api_key` in the draft; the report points at auth.json.
    assert!(!stdout.contains("api_key"), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert_eq!(config.tui.theme.as_deref(), Some("dark"));
    assert_eq!(config.compaction.auto, Some(false));
    assert_eq!(
        config.model.headers,
        vec![("x-tenant".to_string(), "team".to_string())]
    );
    assert!(
        config.skipped.is_empty(),
        "only reader-implemented keys: {:?}",
        config.skipped
    );
    // stdout is the draft alone; the report is on stderr.
    assert!(!stdout.contains("[import-pi]"), "{stdout}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    assert!(stderr.contains("x-trace"), "{stderr}");
    assert!(stderr.contains("defaultThinkingLevel"), "{stderr}");
    // Nothing was written.
    assert!(!dir.join("config").join("config.toml").exists());

    // A literal key: the draft references a suggested variable and the value is nowhere.
    let literal = scratch("import-pi-literal");
    let pi = literal.join("pi");
    std::fs::create_dir_all(&pi).unwrap();
    std::fs::write(
        pi.join("settings.json"),
        r#"{ "defaultProvider": "ollama" }"#,
    )
    .unwrap();
    std::fs::write(
        pi.join("models.json"),
        r#"{ "providers": { "ollama": {
             "baseUrl": "http://localhost:11434/v1",
             "api": "openai-completions",
             "apiKey": "sk-live-abcdef1234567890",
             "models": [{ "id": "qwen2.5-coder:7b" }] } } }"#,
    )
    .unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let _ = radian_host::parse_config(Path::new("/tmp/draft.toml"), &stdout).unwrap();
    assert!(!stdout.contains("api_key"), "{stdout}");
    assert!(stderr.contains("OLLAMA_API_KEY"), "{stderr}");
    assert!(!stdout.contains("sk-live"), "{stdout}");
    assert!(!stderr.contains("sk-live"), "{stderr}");
}

/// Acceptance 2 (second half): a `!command` value is listed and **not executed**.
#[tokio::test]
async fn config_import_pi_lists_a_command_key_and_does_not_run_it() {
    let dir = scratch("import-pi-command");
    let pi = dir.join("pi");
    std::fs::create_dir_all(&pi).unwrap();
    let marker = dir.join("PWNED");
    std::fs::write(pi.join("settings.json"), "{}").unwrap();
    std::fs::write(
        pi.join("models.json"),
        format!(
            r#"{{ "providers": {{ "acme": {{
                 "baseUrl": "https://acme.example/v1",
                 "api": "openai-completions",
                 "apiKey": "!touch {}",
                 "models": [{{ "id": "m" }}] }} }} }}"#,
            marker.display()
        ),
    )
    .unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!marker.exists(), "the `!command` must not have run");
    assert!(stderr.contains("!command"), "{stderr}");
    assert!(!stdout.contains("api_key"), "no key was migrated: {stdout}");
}

/// Acceptance 3: `auth.json` is never read. A file that cannot be read at all (mode `0000`)
/// must not make the migration fail, and its bytes must not appear anywhere.
#[cfg(unix)]
#[tokio::test]
async fn config_import_pi_never_reads_auth_json() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("import-pi-auth");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let auth = pi.join("auth.json");
    std::fs::write(&auth, r#"{"openai":{"type":"api_key","key":"sk-SECRET"}}"#).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o000)).unwrap();

    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("SECRET") && !stderr.contains("SECRET"));
    assert!(stderr.contains("auth.json"), "{stderr}");
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Acceptance 4: `--write` creates the file owner-only and refuses to overwrite an existing
/// one (ADR 0008 §4: this version has no faithful write-back).
#[cfg(unix)]
#[tokio::test]
async fn config_import_pi_write_creates_the_file_and_refuses_to_overwrite() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("import-pi-write");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let home_arg = home.to_str().unwrap();
    let pi_arg = pi.to_str().unwrap();

    let (code, _, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi_arg, "--write"],
            &[("RADIAN_HOME", home_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let target = home.join("config").join("config.toml");
    assert!(stderr.contains(&target.display().to_string()), "{stderr}");
    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the file is owner-only");
    let text = std::fs::read_to_string(&target).unwrap();
    radian_host::parse_config(&target, &text).expect("the written file reads back");

    let before = std::fs::read(&target).unwrap();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", pi_arg, "--write"],
            &[("RADIAN_HOME", home_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(std::fs::read(&target).unwrap(), before, "bytes unchanged");
}

/// Acceptance 5: `--from` is required (a clap usage error, exit 2) and a missing directory is
/// a failure (exit 1), not a silent empty draft.
#[tokio::test]
async fn config_import_pi_requires_from_and_an_existing_directory() {
    let (code, _, stderr) =
        feed_and_wait(spawn_radian_top(&["config", "import-pi"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--from"), "{stderr}");

    let dir = scratch("import-pi-missing");
    let missing = dir.join("nope");
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &["config", "import-pi", "--from", missing.to_str().unwrap()],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("nope"), "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
}

/// `radian config import-claude`: a dry run drafts user + project skills, translates hooks
/// and reports the waits on stderr without writing; `--write` merges the config (never
/// overwrites) and creates the skills and `AGENTS.md` (with the trust-gate notice); an
/// existing `AGENTS.md`/`SKILL.md` is skipped, a second `--write` adds nothing, and there
/// is no `--force` flag.
#[tokio::test]
async fn config_import_claude_drafts_and_writes() {
    let dir = scratch("import-claude");
    let home = dir.join("claude-home");
    std::fs::create_dir_all(home.join("commands")).unwrap();
    std::fs::create_dir_all(home.join("skills")).unwrap();
    std::fs::write(
        home.join("commands/review.md"),
        "---\ndescription: Review a PR diff.\n---\n\nReview $ARGUMENTS now.\n",
    )
    .unwrap();
    std::fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers": {
            "secret-srv": {"command": "node", "env": {"API_KEY": "sk-test-please-ignore-0000"}}}}"#,
    )
    .unwrap();
    std::fs::write(
        home.join("settings.json"),
        r#"{"hooks": {
            "PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo pre"}]},
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo late", "async": true}]}
            ]}}"#,
    )
    .unwrap();
    std::fs::write(home.join("CLAUDE.md"), "# Mine\n").unwrap();
    let proj = dir.join("proj");
    std::fs::create_dir_all(proj.join(".claude/commands")).unwrap();
    std::fs::create_dir_all(proj.join(".claude/skills/team")).unwrap();
    std::fs::write(
        proj.join(".claude/commands/deploy.md"),
        "---\ndescription: Deploy.\n---\n\nDeploy $ARGUMENTS.\n",
    )
    .unwrap();
    std::fs::write(
        proj.join(".claude/skills/team/SKILL.md"),
        "---\nname: team\ndescription: Team.\n---\n\nTeam.\n",
    )
    .unwrap();
    std::fs::write(proj.join(".mcp.json"), r#"{"mcpServers": {}}"#).unwrap();
    std::fs::write(
        proj.join(".claude/settings.json"),
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash",
            "hooks": [{"type": "command", "command": "echo proj"}]}]}}"#,
    )
    .unwrap();
    let home_arg = home.to_str().unwrap().to_owned();
    let proj_arg = proj.to_str().unwrap().to_owned();

    // Dry run: the draft on stdout, the report on stderr, nothing written.
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian_top(
            &[
                "config",
                "import-claude",
                "--claude-home",
                &home_arg,
                "--project",
                &proj_arg,
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("schema_version"), "{stdout}");
    assert!(stdout.contains("Review $ARGUMENTS now."), "{stdout}");
    assert!(!stdout.contains("sk-test-please-ignore"), "{stdout}");
    assert!(stderr.contains("[import-claude]"), "{stderr}");
    assert!(stderr.contains("`async`"), "{stderr}");
    assert!(
        stderr.contains("imported 1 stdio server(s) into `[mcp]`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("imported 1 hook entry as project hooks"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was written"), "{stderr}");

    // `--write`: the config is merged (a comment and an unrelated key survive), the
    // skills and `AGENTS.md` land, with the trust-gate notice for project files.
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let out_arg = out.to_str().unwrap().to_owned();
    let config_path = out.join("config/config.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::write(
        &config_path,
        "# keep me\nschema_version = 1\n[model]\nmodel = \"m-1\"\n",
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_top(
            &[
                "config",
                "import-claude",
                "--claude-home",
                &home_arg,
                "--project",
                &proj_arg,
                "--write",
            ],
            &[("RADIAN_HOME", &out_arg), ("HOME", &out_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("need your trust first"), "{stderr}");
    let text = std::fs::read_to_string(&config_path).unwrap();
    radian_host::parse_config(&config_path, &text).expect("the written file reads back");
    assert!(text.contains("# keep me"), "{text}");
    assert!(text.contains("model = \"m-1\""), "{text}");
    assert!(text.contains("echo pre"), "{text}");
    assert!(text.contains("secret-srv"), "{text}");
    assert!(!text.contains("sk-test-please-ignore"), "{text}");
    assert!(
        out.join(".agents/skills/review/SKILL.md").exists(),
        "{stderr}"
    );
    assert!(out.join("AGENTS.md").exists(), "{stderr}");
    assert!(
        proj.join(".agents/skills/deploy/SKILL.md").exists(),
        "{stderr}"
    );
    assert!(
        proj.join(".agents/skills/team/SKILL.md").exists(),
        "{stderr}"
    );
    // Project hooks translated (`Bash`→`shell`) into the runtime's project hooks file.
    let hooks_text = std::fs::read_to_string(proj.join(".radian/hooks.json")).unwrap();
    assert!(hooks_text.contains("\"shell\""), "{hooks_text}");
    assert!(!hooks_text.contains("Bash"), "{hooks_text}");
    assert!(hooks_text.contains("echo proj"), "{hooks_text}");

    // A second `--write` merges again: nothing is duplicated, existing files are skipped
    // with one line each, and the exit is still 0.
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_top(
            &[
                "config",
                "import-claude",
                "--claude-home",
                &home_arg,
                "--write",
            ],
            &[("RADIAN_HOME", &out_arg), ("HOME", &out_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("exists, not written"), "{stderr}");
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(text.matches("echo pre").count(), 1, "{text}");

    // There is no `--force` flag: it is rejected as an unknown argument (exit 2).
    let (code, _, stderr) = feed_and_wait(
        spawn_radian_top(
            &[
                "config",
                "import-claude",
                "--claude-home",
                &home_arg,
                "--write",
                "--force",
            ],
            &[("RADIAN_HOME", &out_arg), ("HOME", &out_arg)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");

    // No source is a usage error.
    let (code, _, stderr) =
        feed_and_wait(spawn_radian_top(&["config", "import-claude"], &[]), "").await;
    assert_eq!(code, 2, "{stderr}");
}

/// Acceptance 6: a closed stdout is a normal stop (D21), not a panic.
#[cfg(unix)]
#[test]
fn config_import_pi_survives_a_closed_stdout() {
    let dir = scratch("import-pi-sigpipe");
    let pi = dir.join("pi");
    write_pi_fixture(&pi);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = std::process::Command::new(common::radian_bin())
        .args(["config", "import-pi", "--from", pi.to_str().unwrap()])
        .env("RADIAN_HOME", home.as_os_str())
        .env("HOME", home.as_os_str())
        .stdin(Stdio::null())
        .stdout(closed_stdout())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agent");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let status = child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "stderr {stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

/// ADR 0024 Q5: `agent` has no interactive user, so it does **not** register `ask_user` and
/// says so on its startup line. The TUI is the only host that registers it.
#[tokio::test]
async fn the_startup_line_says_ask_user_is_off() {
    let (_dir, ws) = budget_workspace("ask-off");
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("ask_user: off (no interactive user)"),
        "the startup line must say ask_user is off: {stderr}"
    );
    // The model was never offered the tool: check the advertised tool list, not the raw
    // request body, because a tool name can also appear in system-prompt text or in another
    // tool's description.
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{stderr}");
    let names = advertised_tools(&server, 0);
    assert!(
        !names.iter().any(|name| name == "ask_user"),
        "the CLI must not advertise the ask_user tool: {names:?}"
    );
}

// ---------- named providers (ADR 0026 D10) ----------

/// Writes an `auth.json` whose entries are **named providers**: each name carries its own
/// key and its own endpoint in `env` (ADR 0026 D10).
fn write_named_auth_json(root: &Path, entries: &[(&str, &str, &str)]) {
    let dir = root.join("credentials");
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut map = serde_json::Map::new();
    for (name, key, base_url) in entries {
        map.insert(
            (*name).to_string(),
            serde_json::json!({
                "type": "api_key",
                "key": key,
                "env": { "RADIAN_BASE_URL": base_url, "RADIAN_KIND": "openai-compatible" }
            }),
        );
    }
    let path = dir.join("auth.json");
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Two custom endpoints, two keys: each named provider starts against its own mock server and
/// sends **its own** key (D10's whole point — the slots are no longer shared). And when the
/// config also declares the same name, the config wins with a one-line warning.
#[tokio::test]
async fn named_providers_keep_separate_keys_and_config_wins_over_auth() {
    // Two replies each: the third sub-test reuses the mocks (the config-wins run).
    let corp = MockServer::start(vec![text_reply("CORP"), text_reply("CORP")]).await;
    let relay = MockServer::start(vec![text_reply("RELAY"), text_reply("RELAY")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(
        &home,
        &[
            ("corp", "sk-corp-111", &corp.base_url),
            ("relay", "sk-relay-222", &relay.base_url),
        ],
    );
    let envs = |extra: &[(&str, &str)]| {
        let mut all: Vec<(String, String)> = vec![
            ("RADIAN_HOME".into(), home.to_string_lossy().into_owned()),
            (
                "RADIAN_SESSION_DIR".into(),
                fresh_session_dir().to_string_lossy().into_owned(),
            ),
            (
                "HOME".into(),
                fresh_session_dir().to_string_lossy().into_owned(),
            ),
            ("RADIAN_CATALOG_URL".into(), common::fast_catalog_url()),
        ];
        all.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        all
    };

    // `corp` talks to its own endpoint with its own key.
    let child = spawn_radian_raw(
        &["--provider", "corp", "--model", "m-1", "-p", "hi"],
        &envs(&[]),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "CORP\n");
    let requests = corp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-corp-111"),
        "corp must send its own key"
    );
    assert_eq!(relay.requests().len(), 0, "relay is untouched");

    // `relay` likewise, and `corp` still sees exactly one request.
    let child = spawn_radian_raw(
        &["--provider", "relay", "--model", "m-1", "-p", "hi"],
        &envs(&[]),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "RELAY\n");
    let requests = relay.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-relay-222"),
        "relay must send its own key"
    );
    assert_eq!(corp.requests().len(), 1, "relay ran, corp did not");

    // The config's `[providers.corp]` wins over the auth.json entry of the same name, with a
    // warning naming the conflict (D10). The config points `corp` at the *relay* mock.
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config = config_dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[model]\nprovider = \"corp\"\nmodel = \"m-1\"\n\
             [providers.corp]\nkind = \"openai-compatible\"\nbase_url = \"{}\"\n",
            relay.base_url
        ),
    )
    .unwrap();
    let child = spawn_radian_raw(&["--model", "m-1", "-p", "hi"], &envs(&[]));
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "RELAY\n", "the config's base_url won");
    assert!(
        stderr.contains("[provider]"),
        "the conflict must warn once: {stderr}"
    );
    assert!(stderr.contains("corp"), "{stderr}");
}

/// A `spawn_radian` that takes the environment verbatim (the helper above derives it from key
/// env vars; named providers carry their own in `auth.json`).
fn spawn_radian_raw(args: &[&str], envs: &[(String, String)]) -> Child {
    Command::new(common::radian_bin())
        .arg("exec")
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn radian")
}

// ---------- every provider table at startup (ADR 0026 D10) ----------

/// The environment a named-provider run needs: its own `~/.radian` root and session directory,
/// a fresh `HOME`, and the fast-failing catalog sink. `RADIAN_CONFIG` is not set, so the
/// config can only come from `RADIAN_HOME/config/config.toml`.
fn provider_home_envs(home: &Path) -> Vec<(String, String)> {
    vec![
        ("RADIAN_HOME".into(), home.to_string_lossy().into_owned()),
        (
            "HOME".into(),
            fresh_session_dir().to_string_lossy().into_owned(),
        ),
        (
            "RADIAN_SESSION_DIR".into(),
            fresh_session_dir().to_string_lossy().into_owned(),
        ),
        ("RADIAN_CATALOG_URL".into(), common::fast_catalog_url()),
    ]
}

/// Writes `<home>/config/config.toml` (the file `RADIAN_HOME` selects).
fn write_home_config(home: &Path, text: &str) {
    let dir = home.join("config");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
}

/// A `[providers.orphan]` rules-only table is checked at **startup**, whichever provider the run
/// selects: with no `auth.json` entry named `orphan` it is refused with one line naming
/// `providers.orphan`, exit code 2 like every other config refusal, and no request is sent.
#[tokio::test]
async fn a_rules_only_provider_table_without_an_auth_entry_is_refused_at_startup() {
    let server = MockServer::start(vec![text_reply("SHOULD-NOT-RUN")]).await;
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 1\n",
            "[model]\n",
            "provider = \"openai-compatible\"\n",
            "model = \"m\"\n",
            "[providers.orphan]\n",
            "prefer = [\"anthropic\"]\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--base-url", &server.base_url, "-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        stderr.contains("adds to an `auth.json` provider that does not exist"),
        "{stderr}"
    );
    assert!(
        server.requests().is_empty(),
        "a refused table must send no request: {stderr}"
    );
}

/// `reasoning_replay` alone is rules-only by the documented definition, so an orphan table with
/// only that key is refused at startup too.
#[tokio::test]
async fn a_reasoning_replay_only_provider_table_without_an_auth_entry_is_refused_at_startup() {
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 1\n",
            "[model]\n",
            "provider = \"openai-compatible\"\n",
            "model = \"m\"\n",
            "[providers.orphan]\n",
            "reasoning_replay = \"reasoning_content\"\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
}

/// With the same-name `auth.json` entry, the same rules-only table starts **while the run
/// selects another provider**, and adds no "config wins" warning (an overlay never warns). The
/// same config without the entry is refused by the same startup check, so this asserts the
/// startup-wide check: without it the second run starts and the turn runs.
#[tokio::test]
async fn a_rules_only_provider_table_with_an_auth_entry_starts_while_another_provider_is_selected()
{
    let server = MockServer::start(vec![text_reply("BUILT-IN"), text_reply("BUILT-IN")]).await;
    let orphan = MockServer::start(vec![text_reply("ORPHAN")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(
        &home,
        &[
            ("openai-compatible", "sk-test", &server.base_url),
            ("orphan", "sk-orphan-1", &orphan.base_url),
        ],
    );
    write_home_config(
        &home,
        "schema_version = 1\n[providers.orphan]\nprefer = [\"anthropic\"]\n",
    );
    let args = ["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"];
    let child = spawn_radian_raw(&args, &provider_home_envs(&home));
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "BUILT-IN\n");
    assert!(
        !stderr.contains("declared in both config and auth.json"),
        "an overlay must not warn: {stderr}"
    );
    assert!(
        !stderr.contains("adds to an `auth.json` provider"),
        "{stderr}"
    );
    assert!(
        orphan.requests().is_empty(),
        "the unselected overlay's provider must not be resolved: {stderr}"
    );

    // The same table, the same selected provider, without the entry: refused at startup.
    write_named_auth_json(&home, &[("openai-compatible", "sk-test", &server.base_url)]);
    let child = spawn_radian_raw(&args, &provider_home_envs(&home));
    let (code, _, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        stderr.contains("adds to an `auth.json` provider that does not exist"),
        "{stderr}"
    );
}

/// A full declaration (`kind` + `base_url`) needs no `auth.json` entry: the startup check skips
/// it, so with a rules-only table beside it the refusal names only the rules-only one, and with
/// the full table alone the run selects another provider and starts. Dropping the `is_full`
/// guard makes the check refuse `corp` instead.
#[tokio::test]
async fn a_full_provider_table_without_an_auth_entry_is_not_refused_at_startup() {
    let server = MockServer::start(vec![text_reply("BUILT-IN")]).await;
    let corp = MockServer::start(vec![text_reply("CORP")]).await;
    let home = fresh_session_dir();
    write_named_auth_json(&home, &[("openai-compatible", "sk-test", &server.base_url)]);
    let full = format!(
        "schema_version = 1\n[providers.corp]\nkind = \"openai-compatible\"\n\
         base_url = \"{}\"\n",
        corp.base_url
    );

    // Both tables, neither name in `auth.json`: the refusal names the rules-only table only.
    write_home_config(
        &home,
        &format!("{full}[providers.orphan]\nprefer = [\"anthropic\"]\n"),
    );
    let child = spawn_radian_raw(
        &["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"],
        &provider_home_envs(&home),
    );
    let (code, _, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(
        !stderr.contains("providers.corp"),
        "a full declaration is not rules-only: {stderr}"
    );

    // The full table alone: it starts while another provider is selected, and is never resolved.
    write_home_config(&home, &full);
    let child = spawn_radian_raw(
        &["--base-url", &server.base_url, "--model", "m-1", "-p", "hi"],
        &provider_home_envs(&home),
    );
    let (code, stdout, stderr) = feed_and_wait(child, "").await;
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    assert_eq!(stdout, "BUILT-IN\n");
    assert!(
        corp.requests().is_empty(),
        "the unselected full declaration must not be resolved: {stderr}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "{stderr}");
    assert_eq!(requests[0].json()["model"], "m-1");
}

/// A read-only configuration (a newer `schema_version`) gets the same refusal as every other
/// provider-table refusal: the reader's own provider errors refuse in read-only mode too, and
/// this check runs after the parse.
#[tokio::test]
async fn a_read_only_config_with_an_orphan_rules_only_table_is_refused_at_startup() {
    let dir = fresh_session_dir();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        concat!(
            "schema_version = 2\n",
            "[providers.orphan]\n",
            "prefer = [\"anthropic\"]\n",
        ),
    )
    .unwrap();
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["-p", "hi"],
            &[
                ("RADIAN_CONFIG", config.to_str().unwrap()),
                ("OPENAI_API_KEY", "sk-test"),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("providers.orphan"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
}

// ---------- radian login / radian logout (ADR 0026 D3) ----------

/// `radian login` refuses without a terminal: a key pasted into a pipe would be visible, so
/// the message points at `auth.json` instead (D3). Exit code 2 (usage-shaped).
#[tokio::test]
async fn login_without_a_terminal_refuses_and_points_at_auth_json() {
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["login", "openai"])
        .env("HOME", fresh_session_dir())
        .env("RADIAN_HOME", fresh_session_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("spawn radian login");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("login needs a terminal"), "{stderr}");
    assert!(stderr.contains("auth.json"), "{stderr}");
    // The refusal must not echo the provider's default endpoint or any placeholder.
    assert!(!stderr.contains("sk-"), "{stderr}");
}

/// A private `RADIAN_HOME` with a two-key `opencode-go` entry (ADR 0026 D1).
fn home_with_two_keys() -> PathBuf {
    let home = fresh_session_dir();
    let credentials = home.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credentials, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let auth = credentials.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"opencode-go": {"type": "api_key", "key": "sk-first",
                            "keys": [{"name": "team", "key": "sk-second"}]}}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    home
}

/// `radian logout <provider> <name>` removes that key without asking; removing the first key
/// promotes the next one to `key` (ADR 0026 D1/D3).
#[tokio::test]
async fn logout_with_a_key_name_removes_one_and_promotes_the_next() {
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "team"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-first");
    assert!(auth["opencode-go"].get("keys").is_none());

    // The first key: the next one is promoted to `key`, so pi keeps reading the entry.
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "first"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-second");
}

/// An unknown key name is refused, naming the known ones, and nothing is removed.
#[tokio::test]
async fn logout_with_an_unknown_key_name_names_the_known_ones() {
    let home = home_with_two_keys();
    let output = tokio::process::Command::new(common::radian_bin())
        .args(["logout", "opencode-go", "no-such-key"])
        .env("HOME", &home)
        .env("RADIAN_HOME", &home)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .expect("spawn radian logout");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no-such-key"), "{stderr}");
    assert!(
        stderr.contains("first") && stderr.contains("team"),
        "{stderr}"
    );
    let auth: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(auth["opencode-go"]["key"], "sk-first");
    assert_eq!(auth["opencode-go"]["keys"][0]["name"], "team");
}

/// Printed by the pty's shell after its trailing `stty -a`, so a reader that waits for it knows
/// it has the whole tty state (see [`PtyChild::wait_for_stty`]).
const STTY_END: &str = "STTY-END";

/// A minimal pty: the child runs `sh -c '"$@"; echo EXITED:$?; stty -a; echo STTY-END'`, so
/// the same pty reports the exit code and the terminal state **after** the app left (the
/// echo-restore invariant is only observable there). Raw bytes accumulate; the reader drains
/// them.
struct PtyChild {
    child: tokio::process::Child,
    master: std::fs::File,
}

impl PtyChild {
    fn spawn(args: &[&str], home: &Path) -> Self {
        let mut master_fd: libc::c_int = -1;
        let mut slave_fd: libc::c_int = -1;
        // SAFETY: openpty writes the two fds; the name and termios pointers may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        // One dup per stdio slot: each raw fd is owned by exactly one `Stdio`, and the
        // `Command` closes both after spawn (the child's exec copies keep the slave open
        // until it exits, which is what gives the master its EOF).
        let stdout_fd = unsafe { libc::dup(slave_fd) };
        assert!(stdout_fd >= 0, "dup failed");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!(
                r#""$@"; echo "EXITED:$?"; stty -a; echo "{STTY_END}""#
            ))
            .arg("sh")
            .args(args)
            .env("HOME", home)
            .env("RADIAN_HOME", home)
            .env("RADIAN_SESSION_DIR", fresh_session_dir())
            .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
            .env_remove("RADIAN_CONFIG")
            .stdin(unsafe {
                // SAFETY: slave_fd is a fresh owned fd, moved into the Command.
                use std::os::fd::FromRawFd;
                std::process::Stdio::from_raw_fd(slave_fd)
            })
            .stdout(unsafe {
                // SAFETY: stdout_fd is a fresh owned fd, moved into the Command.
                use std::os::fd::FromRawFd;
                std::process::Stdio::from_raw_fd(stdout_fd)
            })
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn in pty");
        // Non-blocking master, so `drain` never blocks (the first frame is bigger than the
        // pty buffer, and the app would otherwise block in its draw).
        // SAFETY: master_fd is a fresh fd; F_SETFL is async-signal-safe.
        unsafe { libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK) };
        // SAFETY: master_fd is a fresh fd.
        let master: std::fs::File = unsafe { std::os::fd::FromRawFd::from_raw_fd(master_fd) };
        Self { child, master }
    }

    /// Drains whatever is pending.
    fn drain(&mut self, buf: &mut Vec<u8>) {
        use std::io::Read;
        let mut chunk = [0u8; 8192];
        if let Ok(n) = self.master.read(&mut chunk) {
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn wait_for(&mut self, buf: &mut Vec<u8>, needle: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            self.drain(buf);
            if String::from_utf8_lossy(buf).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the shell's trailing `stty -a` is complete and returns its text.
    ///
    /// `stty -a` writes to a tty line by line, and [`Self::drain`] copies at most one chunk
    /// per call, so its first line (`speed …`) can be in `buf` while the line that carries
    /// `icanon` and `echo` has not been read yet: asserting on that prefix fails on a healthy
    /// terminal under load. The shell prints [`STTY_END`] after `stty -a`, so this waits for
    /// that marker — everything before it is complete — and returns what followed the app's
    /// `EXITED:<code>` line.
    async fn wait_for_stty(
        &mut self,
        buf: &mut Vec<u8>,
        exited: &str,
        secs: u64,
    ) -> Option<String> {
        if !self.wait_for(buf, STTY_END, secs).await {
            return None;
        }
        let text = String::from_utf8_lossy(buf).into_owned();
        let after = text.split(exited).nth(1)?;
        Some(after.split(STTY_END).next().unwrap_or(after).to_owned())
    }

    fn write(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut master = &self.master;
        let _ = master.write_all(bytes);
        let _ = master.flush();
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// `stty -a` shows the echo state as tokens (`echo`, `-echo`, `echoke`, …): check the tokens,
/// not substrings (`-echok` contains `-echo`).
fn assert_stty_has_echo(stty: &str) {
    let tokens: Vec<&str> = stty
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    assert!(
        tokens.contains(&"icanon"),
        "canonical mode must be restored: {stty}"
    );
    assert!(
        tokens.contains(&"echo"),
        "echo must be restored (and `-echo` absent): {stty}"
    );
    assert!(!tokens.contains(&"-echo"), "echo is still off: {stty}");
}

/// The full `radian login custom` flow on a real pty: the key is **never** in the pty stream
/// (echo off), the verification runs against a local server, `auth.json` gains the entry
/// (with the endpoint in `env`, D10), and after the app exits the terminal's echo is **on**
/// again — the restore held on the normal exit path.
#[tokio::test]
async fn pty_agent_login_custom_hides_the_key_and_restores_echo() {
    use std::io::Write as _;
    // The verification endpoint: a one-shot 200 per connection.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 8192];
            let _ = std::io::Read::read(&mut stream, &mut buf);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        }
    });

    let home = fresh_session_dir();
    let credentials = home.join("credentials");
    std::fs::create_dir_all(&credentials).unwrap();
    let mut pty = PtyChild::spawn(&[&common::radian_bin_str(), "login", "custom"], &home);
    let mut seen = Vec::new();
    const KEY: &str = "sk-CLI-HIDDEN-99";
    assert!(
        pty.wait_for(&mut seen, "provider name", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    pty.write(b"corp\r");
    assert!(pty.wait_for(&mut seen, "protocol", 10).await);
    pty.write(b"1\r");
    assert!(pty.wait_for(&mut seen, "base URL", 10).await);
    pty.write(format!("http://{addr}\r").as_bytes());
    assert!(pty.wait_for(&mut seen, "API key", 10).await);
    pty.write(format!("{KEY}\r").as_bytes());
    assert!(
        pty.wait_for(&mut seen, "saved the key for `corp`", 10)
            .await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        pty.wait_for(&mut seen, "EXITED:0", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );

    // The key never crossed the wire in a visible form.
    assert!(
        !String::from_utf8_lossy(&seen).contains(KEY),
        "the key leaked: {}",
        String::from_utf8_lossy(&seen)
    );
    // The record is right, next to nothing else, 0600.
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("credentials/auth.json")).unwrap())
            .unwrap();
    assert_eq!(saved["corp"]["key"], KEY);
    assert_eq!(
        saved["corp"]["env"]["RADIAN_BASE_URL"],
        format!("http://{addr}")
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(home.join("credentials/auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    // Echo restored after the exit: `stty -a` must show `echo` enabled. Wait for the whole
    // `stty` output (its end marker), not just `EXITED:0` or stty's first line: the pty
    // delivers those bytes in several writes.
    let stty = pty
        .wait_for_stty(&mut seen, "EXITED:0", 5)
        .await
        .unwrap_or_else(|| panic!("no stty output: {}", String::from_utf8_lossy(&seen)));
    assert_stty_has_echo(&stty);
}

/// Ctrl-C **during the hidden key prompt** ends the process (130) and still restores the
/// echo — the invariant the brief calls out by name.
#[tokio::test]
async fn pty_agent_login_ctrl_c_during_the_key_prompt_restores_echo() {
    let home = fresh_session_dir();
    std::fs::create_dir_all(home.join("credentials")).unwrap();
    let mut pty = PtyChild::spawn(&[&common::radian_bin_str(), "login", "anthropic"], &home);
    let mut seen = Vec::new();
    assert!(
        pty.wait_for(&mut seen, "API key", 10).await,
        "{}",
        String::from_utf8_lossy(&seen)
    );
    pty.write(b"sk-partial");
    tokio::time::sleep(Duration::from_millis(100)).await;
    pty.write(&[0x03]); // Ctrl-C inside the echo-off window
    assert!(
        pty.wait_for(&mut seen, "EXITED:130", 10).await,
        "Ctrl-C did not end the login: {}",
        String::from_utf8_lossy(&seen)
    );
    // The terminal is back to cooked mode: echo on, canonical on. The whole `stty -a` output
    // is only in `seen` at its end marker.
    let stty = pty
        .wait_for_stty(&mut seen, "EXITED:130", 5)
        .await
        .unwrap_or_else(|| panic!("no stty output: {}", String::from_utf8_lossy(&seen)));
    assert_stty_has_echo(&stty);
}

/// ADR 0043: a headless `radian exec` mounts neither `git_commit` nor any task/lead/
/// named tool; its list is the built-in tools only.
#[tokio::test]
async fn a_headless_exec_mounts_no_orchestration_tools() {
    let server = MockServer::start(vec![text_reply("hi")]).await;
    // A private home: the tool set must not depend on the machine's real memory.
    let home = scratch("headless-tools");
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_HOME", home.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let names = advertised_tools(&server, 0);
    assert!(
        !names.iter().any(|name| name == "git_commit"),
        "`git_commit` must not be mounted: {names:?}"
    );
    for gone in [
        "spawn_task",
        "wait_task",
        "wait_any",
        "list_tasks",
        "task_status",
        "cancel_task",
        "remove_task",
        "integrate_task",
        "answer_task",
        "deny_task_approval",
        "escalate_approval",
        "send_agent",
        "wait_agent",
    ] {
        assert!(
            !names.iter().any(|name| name == gone),
            "`{gone}` must not be mounted: {names:?}"
        );
    }
    // The exact set: a headless session with no `--workspace` and an empty memory mounts the
    // built-in `skill` (its `theme-import` skill is compiled in), `subagent` (ADR 0043
    // decision 1a — a parent mounts it even with no workspace; only `explore` needs one) and
    // `goal` (ADR 0047 — host-level like `ask_user`, answering `no_active_goal` without one),
    // and nothing else. No memory tool exists: `memory_search`, `memory_save` and
    // `memory_revise` are unknown names.
    let mut got = names.clone();
    got.sort();
    assert_eq!(
        got,
        ["goal", "skill", "subagent"],
        "the exact tool set of a fresh headless session: {names:?}"
    );
}

/// The intent-stop validator end to end (backlog `turn-end-continuation.md` §B
/// acceptance): a turn ending with "announcing a next step but no tool call" opens
/// exactly one more round carrying the fixed continuation text; removing the
/// validator (or the mock's second reply going unused) fails the request count.
#[tokio::test]
async fn intent_stop_continues_an_intent_only_turn_end_once() {
    let dir = scratch("intent-stop-fire");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        text_reply("Now the docs-only ADR updates."),
        text_reply("Done."),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        2,
        "one continuation opens exactly one more round"
    );
    let second = last_user_text(&messages_of(&server, 1));
    assert!(
        second.contains(radian_host::intent_stop::INTENT_STOP_TEXT),
        "the fixed continuation text rides the new round: {second}"
    );
}

/// Ordinary turn ends hold the validator end to end: a done report, a question, and a
/// long report whose last sentence matches the rule each end the run with no further
/// round. A validator that fires on any of them adds a request and fails this.
#[tokio::test]
async fn ordinary_turn_ends_do_not_continue() {
    for end in [
        "Done.".to_owned(),
        "Is that all you need?".to_owned(),
        format!("{}\n\nNext: the reviewer decides.", "x".repeat(300)),
    ] {
        let dir = scratch("intent-stop-hold");
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let server = MockServer::start(vec![text_reply(&end)]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    ws.to_str().unwrap(),
                    "-p",
                    "hi",
                ],
                &[],
            ),
            "",
        )
        .await;
        assert_eq!(code, 0, "{end}: {stderr}");
        assert_eq!(server.requests().len(), 1, "no continuation after {end:?}");
    }
}

/// A waiting text holds while its background job runs: the notice — not the validator —
/// opens the next round. The validator fires within milliseconds of the turn end while
/// the notice needs the job's two seconds, so a broken suppression shows up as a
/// fourth request carrying the fixed text (or a run out of mock replies).
#[tokio::test]
async fn waiting_on_a_running_job_does_not_continue() {
    let dir = scratch("intent-stop-waiting");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "c1",
            "shell",
            &json!({"command": "sleep 2", "background": true}).to_string(),
        ),
        text_reply("Waiting for the completion notice."),
        text_reply("Done."),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "prompt, post-tool and post-notice rounds, no validator round"
    );
    for (index, request) in requests.iter().enumerate() {
        assert!(
            !request.json().to_string().contains("announced a next step"),
            "request {index} must not carry the validator text"
        );
    }
}

/// `[hooks] intent_stop = false` turns the built-in validator off end to end: an
/// intent-only turn end opens no further round.
#[tokio::test]
async fn intent_stop_off_in_config_does_not_continue() {
    let dir = scratch("intent-stop-off");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let config = dir.join("radian.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[hooks]\nintent_stop = false\n",
    )
    .unwrap();
    let server = MockServer::start(vec![text_reply("Now the docs-only ADR updates.")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &[("RADIAN_CONFIG", config.to_str().unwrap())],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        server.requests().len(),
        1,
        "the switch turns the validator off"
    );
}

// ---------- project trust (`radian trust status|on|off|forget|list`) ----------

/// Runs `radian trust <args>` with `RADIAN_HOME` (and `HOME`) pinned to a scratch root,
/// capturing stdout and stderr. Returns the exit code and both streams.
fn run_trust(home: &Path, ws: &Path, args: &[&str]) -> (i32, String, String) {
    let mut full_args = vec!["trust", "--workspace", ws.to_str().unwrap()];
    full_args.extend(args);
    let output = std::process::Command::new(common::radian_bin())
        .args(&full_args)
        .env("RADIAN_HOME", home)
        .env("HOME", home)
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
        .env_remove("RADIAN_CONFIG")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run radian trust");
    (
        output.status.code().unwrap(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `radian trust status|on|off|forget|list` in a temp `RADIAN_HOME`: the output lines,
/// the exit codes, and the file written. The mutation (a write writes nothing) fails
/// the disk and follow-up status assertions.
#[test]
fn trust_commands_read_and_write_the_record() {
    let dir = scratch("trust-cli");
    let home = dir.join("home");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let key = canonical(&ws);

    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout.trim(),
        "trust: no saved decision (untrusted)",
        "{stdout}"
    );

    let (code, stdout, stderr) = run_trust(&home, &ws, &["on"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("takes effect at the next start"),
        "{stdout}"
    );
    assert!(stdout.starts_with("trusted "), "{stdout}");
    let store = std::fs::read_to_string(home.join("trust/trust.json")).unwrap();
    assert!(store.contains(&format!("\"{key}\": true")), "{store}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("trust on (saved for"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["list"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), format!("{key}: on"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["off"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("distrusted "), "{stdout}");
    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("trust off (saved for"), "{stdout}");

    let (code, stdout, stderr) = run_trust(&home, &ws, &["forget"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.contains("takes effect at the next start"),
        "{stdout}"
    );
    let (code, stdout, stderr) = run_trust(&home, &ws, &["status"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stdout.trim(),
        "trust: no saved decision (untrusted)",
        "{stdout}"
    );

    // Forgetting again is a no-op that writes nothing and says so.
    let (code, stdout, stderr) = run_trust(&home, &ws, &["forget"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("no saved trust decision"), "{stdout}");
    let (code, stdout, stderr) = run_trust(&home, &ws, &["list"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout.trim(), "no saved trust decisions", "{stdout}");

    // An unknown subcommand is a usage error, and `exec` runs the same dispatch.
    let output = std::process::Command::new(common::radian_bin())
        .args(["trust", "bogus"])
        .env("RADIAN_HOME", &home)
        .env("HOME", &home)
        .output()
        .expect("run radian trust bogus");
    assert_eq!(output.status.code(), Some(2));
    let exec = std::process::Command::new(common::radian_bin())
        .args([
            "exec",
            "--workspace",
            ws.to_str().unwrap(),
            "trust",
            "status",
        ])
        .env("RADIAN_HOME", &home)
        .env("HOME", &home)
        .output()
        .expect("run radian exec trust status");
    assert_eq!(
        exec.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&exec.stderr)
    );
    assert!(
        String::from_utf8_lossy(&exec.stdout).contains("no saved decision"),
        "{}",
        String::from_utf8_lossy(&exec.stdout)
    );
}

// ---------- RADIAN_EXTRA_BODY (ADR 0026 D13) ----------

/// The environment value reaches the request body through the host, and startup names the
/// keys applied — never their values.
#[tokio::test]
async fn the_extra_body_env_reaches_the_request_and_the_startup_line_names_the_keys() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXTRA_BODY", r#"{"thinking":{"type":"enabled"}}"#)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let body = server.requests()[0].json();
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert!(
        stderr.contains("[body] RADIAN_EXTRA_BODY sets: thinking"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("enabled"),
        "startup names the keys, never their values: {stderr}"
    );
}

/// A reserved key, a non-object and an output cap the reservation cannot hold are startup
/// refusals naming the variable (and the key), before any session file exists. Exit code 2 is
/// the code every startup refusal uses.
#[tokio::test]
async fn a_bad_extra_body_stops_startup_with_the_named_error() {
    for (value, needle) in [
        ("{\"messages\":[]}", "messages"),
        ("[1]", "object"),
        // ADR 0018 §2: the request would send this output while `R` could only hold `u32::MAX`.
        ("{\"max_completion_tokens\":5000000000}", "4294967295"),
    ] {
        let (code, _, stderr) = feed_and_wait(
            spawn_radian(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    "http://127.0.0.1:9/v1",
                    "-p",
                    "hi",
                ],
                &[("RADIAN_EXTRA_BODY", value)],
            ),
            "",
        )
        .await;
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("RADIAN_EXTRA_BODY"), "{value}: {stderr}");
        assert!(stderr.contains(needle), "{value}: {stderr}");
    }
}

/// ADR 0026 D13: reserved keys are checked per route. A chat route accepts `max_tokens` and
/// the captured request carries it; the Anthropic route reserves it, so the same value stops
/// startup with exit 2 and a line naming the variable and the key.
#[tokio::test]
async fn the_extra_body_max_tokens_is_allowed_on_a_chat_route_and_refused_on_anthropic() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, stdout, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#)],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stdout, "ok\n");
    let body = server.requests()[0].json();
    assert_eq!(body["max_tokens"], 32000, "{body}");

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &["--model", "claude-x", "--provider", "anthropic", "-p", "hi"],
            &[
                ("ANTHROPIC_API_KEY", "sk-ant-test-key"),
                ("RADIAN_EXTRA_BODY", r#"{"max_tokens":32000}"#),
            ],
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("RADIAN_EXTRA_BODY"), "{stderr}");
    assert!(stderr.contains("max_tokens"), "{stderr}");
}

// ---------- subtree context files (ADR 0036) ----------

/// A one-call `read` reply.
fn read_reply(id: &str, path: &str) -> Reply {
    tool_reply(id, "read", &json!({"path": path}).to_string())
}

/// A one-call `grep` reply.
fn grep_reply(id: &str, pattern: &str, path: &str) -> Reply {
    tool_reply(
        id,
        "grep",
        &json!({"pattern": pattern, "path": path}).to_string(),
    )
}

/// A one-call `glob` reply.
fn glob_reply(id: &str, pattern: &str) -> Reply {
    tool_reply(id, "glob", &json!({"pattern": pattern}).to_string())
}

/// Writes a nested file under `dir`, creating its directories.
fn write_nested(dir: &Path, relative: &str, text: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// How often `needle` appears in the n-th recorded request's **conversation** messages. The
/// system prompt is left out, so a startup file and an appended block cannot be confused.
fn conversation_count(server: &MockServer, n: usize, needle: &str) -> usize {
    messages_of(server, n)
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] != "system")
        .map(|message| message.to_string().matches(needle).count())
        .sum()
}

/// The session files' text, concatenated.
fn sessions_text(sessions: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(sessions) else {
        return String::new();
    };
    let mut text = String::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    text
}

/// A workspace and session directory for one subtree test. `$TMPDIR`, not `/tmp`: the tests
/// do not open a control socket, and this keeps the tree writable inside the workspace
/// sandbox. The workspace is its own repository root, so discovery stops there.
fn subtree_scratch(label: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = common::process_dir(&format!("radian-exec-{label}"));
    let ws = dir.join("ws");
    std::fs::create_dir_all(ws.join(".git")).unwrap();
    let sessions = fresh_session_dir();
    (dir, ws, sessions)
}

/// 1 and 2: reading `crates/x/src/a.rs` appends `crates/x/AGENTS.md` once, before the next
/// model request, and the journal has exactly one `ContextFileAppended`; reading
/// `crates/x/src/b.rs` next appends nothing.
#[tokio::test]
async fn a_subtree_context_file_appends_once_before_the_next_request() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-once");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/src/b.rs", "fn b() {}");
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        read_reply("c2", "crates/x/src/b.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 3, "{stderr}");
    assert_eq!(
        conversation_count(&server, 0, "SUBTREE-X-RULES"),
        0,
        "nothing is appended before the first read"
    );
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        1,
        "the block is in the request after the first read"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "the second read appends nothing"
    );

    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        1,
        "{journal}"
    );
    assert!(
        journal.contains("\"path\":\"crates/x/AGENTS.md\""),
        "{journal}"
    );
    assert!(
        journal.contains(&format!("\"bytes\":{}", "SUBTREE-X-RULES".len())),
        "{journal}"
    );
}

/// 2: after `crates/x/AGENTS.md` is loaded, reading `crates/x/sub/c.rs` appends only
/// `crates/x/sub/AGENTS.md`.
#[tokio::test]
async fn a_deeper_subtree_appends_only_the_deeper_file() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-deeper");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/sub/AGENTS.md", "SUBTREE-SUB-RULES");
    write_nested(&ws, "crates/x/sub/c.rs", "fn c() {}");
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        read_reply("c2", "crates/x/sub/c.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-SUB-RULES"),
        0,
        "the deeper file is not loaded yet"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "the parent file stays once"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-SUB-RULES"),
        1,
        "only the deeper file is appended"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        2,
        "{journal}"
    );
    assert!(
        journal.contains("\"path\":\"crates/x/sub/AGENTS.md\""),
        "{journal}"
    );
}

/// 3: the workspace root's `AGENTS.md` is loaded at startup and is never appended again.
#[tokio::test]
async fn a_startup_context_file_is_never_appended_again() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-startup");
    write_nested(&ws, "AGENTS.md", "ROOT-RULES");
    write_nested(&ws, "src/a.rs", "fn a() {}");
    let server = MockServer::start(vec![read_reply("c1", "src/a.rs"), text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "ROOT-RULES"),
        0,
        "the startup file is not appended"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        0,
        "{journal}"
    );
}

/// 5: `[context] subtree_files = false` appends nothing and writes no event.
#[tokio::test]
async fn the_subtree_switch_turns_the_append_off() {
    let (dir, ws, sessions) = subtree_scratch("subtree-off");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[context]\nsubtree_files = false\n",
    );
    let server = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        0,
        "the switch appends nothing"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        0,
        "{journal}"
    );
}

/// 7: a resumed session whose journal already has `ContextFileAppended` for
/// `crates/x/AGENTS.md` does not append it again.
#[tokio::test]
async fn a_resumed_session_does_not_append_a_recorded_file_again() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-resume");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/src/a.rs", "fn a() {}");
    write_nested(&ws, "crates/x/src/b.rs", "fn b() {}");
    let first = MockServer::start(vec![
        read_reply("c1", "crates/x/src/a.rs"),
        text_reply("done"),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &first.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-id",
                "subtree-resume",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        sessions_text(&sessions)
            .matches("\"event\":\"ContextFileAppended\"")
            .count(),
        1
    );

    let second = MockServer::start(vec![
        read_reply("c2", "crates/x/src/b.rs"),
        text_reply("done again"),
    ])
    .await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &second.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--resume",
                "subtree-resume",
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go on",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&second, 0, "SUBTREE-X-RULES"),
        1,
        "the resumed transcript carries the one appended message"
    );
    assert_eq!(
        conversation_count(&second, 1, "SUBTREE-X-RULES"),
        1,
        "the resumed read appends nothing"
    );
    assert_eq!(
        sessions_text(&sessions)
            .matches("\"event\":\"ContextFileAppended\"")
            .count(),
        1
    );
}

/// 9: a `grep` with `path: crates/x` appends `crates/x/AGENTS.md`; a `glob` whose results
/// list files under two subtrees appends both subtrees' files, each once.
#[tokio::test]
async fn grep_paths_and_glob_results_trigger_their_subtrees() {
    let (_dir, ws, sessions) = subtree_scratch("subtree-search");
    write_nested(&ws, "crates/x/AGENTS.md", "SUBTREE-X-RULES");
    write_nested(&ws, "crates/x/a.rs", "fn x() {}");
    write_nested(&ws, "crates/y/AGENTS.md", "SUBTREE-Y-RULES");
    write_nested(&ws, "crates/y/b.rs", "fn y() {}");
    let server = MockServer::start(vec![
        grep_reply("c1", "fn ", "crates/x"),
        glob_reply("c2", "**/*.rs"),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-X-RULES"),
        1,
        "the grep path triggered crates/x"
    );
    assert_eq!(
        conversation_count(&server, 1, "SUBTREE-Y-RULES"),
        0,
        "crates/y was not touched yet"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-X-RULES"),
        1,
        "crates/x stays once"
    );
    assert_eq!(
        conversation_count(&server, 2, "SUBTREE-Y-RULES"),
        1,
        "the glob result triggered crates/y"
    );
    let journal = sessions_text(&sessions);
    assert_eq!(
        journal.matches("\"event\":\"ContextFileAppended\"").count(),
        2,
        "{journal}"
    );
}

/// 10: `[context] other = 1` lands in `skipped`, the session starts, and the startup line
/// names the ignored key.
#[tokio::test]
async fn an_unknown_context_key_is_skipped_and_the_session_starts() {
    let (dir, ws, sessions) = subtree_scratch("subtree-unknown-key");
    let config = write_file(
        &dir,
        "config.toml",
        "schema_version = 1\n[context]\nother = 1\n",
    );
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let (code, _, stderr) = feed_and_wait(
        spawn_radian(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--config",
                config.to_str().unwrap(),
                "--session-dir",
                sessions.to_str().unwrap(),
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 1, "{stderr}");
    assert!(
        stderr.contains("[config] ignored key `context.other`"),
        "{stderr}"
    );
}
