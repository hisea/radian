//! The `agent` CLI against a mock model server and the real MCP fixture, as subprocesses.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The `agent` binary, built by the harness.
fn agent_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent")
}

/// The MCP fixture built next to `agent`. `cargo test -p agent-cli` alone does not
/// necessarily build it, so the failure says what to run.
fn fixture_bin() -> PathBuf {
    let path = Path::new(agent_bin()).parent().unwrap().join("mcp-fixture");
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
    static SESSION_DIRS: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-cli-sessions-{}-{}",
        std::process::id(),
        SESSION_DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_agent(args: &[&str], envs: &[(&str, &str)]) -> Child {
    Command::new(agent_bin())
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", fresh_session_dir())
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn agent")
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

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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
        let mut child = spawn_agent(args, envs);
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
    /// CLI did not need more input to finish. Bounded by `limit`.
    async fn finish_open(self, limit: Duration) -> (i32, String, String) {
        let Self {
            child,
            mut stderr,
            mut stderr_seen,
            stdin,
        } = self;
        let (output, ()) = timeout(limit, async {
            tokio::join!(child.wait_with_output(), async {
                let mut rest = String::new();
                let _ = stderr.read_to_string(&mut rest).await;
                stderr_seen.push_str(&rest);
            })
        })
        .await
        .expect("the CLI did not exit while stdin stayed open");
        let output = output.expect("wait");
        drop(stdin);
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
        spawn_agent(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(stdout, "Hello there\n");
    assert!(!stderr.contains("Hello there"), "{stderr}");
    assert_eq!(messages_of(&server, 0)[0]["content"], "hi");
}

#[tokio::test]
async fn a_model_error_exits_one() {
    let server = MockServer::start(vec![Reply::status(
        401,
        "{\"error\":{\"message\":\"bad key\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_agent(
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

// ---------- usage and configuration errors ----------

#[tokio::test]
async fn usage_errors_exit_two() {
    let (code, _, _) = feed_and_wait(spawn_agent(&["-p", "hi"], &[]), "").await; // no --model
    assert_eq!(code, 2);
}

#[tokio::test]
async fn bad_mcp_config_exits_two_with_the_path() {
    let dir = scratch("config");

    // Unknown field.
    let unknown = write_file(&dir, "unknown.json", "{\"servers\": [], \"bogus\": 1}");
    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "-p",
                "hi",
                "--api-key-env",
                "AGENT_CLI_TEST_KEY",
            ],
            &[("AGENT_CLI_TEST_KEY", KEY)],
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

#[tokio::test]
async fn a_remote_base_url_without_a_key_fails_fast() {
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
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
    assert!(stderr.contains("OPENAI_API_KEY"), "{stderr}");
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
        spawn_agent(
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

// ---------- custom headers ----------

#[tokio::test]
async fn a_header_flag_is_sent_and_can_be_repeated() {
    let server = MockServer::start(vec![text_reply("ok")]).await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
    assert!(stderr.contains("--api-key-env"), "{stderr}");
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
        500,
        "{\"error\":{\"message\":\"bad team-secret-value\"}}",
    )])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_agent(
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
    assert!(stderr.contains("HTTP 500"), "{stderr}");
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
        spawn_agent(
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
            spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
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
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--mcp-config",
                config.to_str().unwrap(),
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
        spawn_agent(
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
        ],
        &[],
    );
    let (code, _, _) = running.finish_open(EXIT_WATCHDOG).await;

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
        spawn_agent(&["--model", "m", "--base-url", &server.base_url], &[]),
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
        spawn_agent(
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
    assert_eq!(
        messages_of(&server, 0)[0],
        json!({"role": "system", "content": "Be brief."})
    );
}

#[tokio::test]
async fn turn_timeout_ends_the_turn() {
    let server = MockServer::start(vec![Reply::SseThenHang { pieces: Vec::new() }]).await;
    let start = Instant::now();

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
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

    assert_eq!(code, 1);
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
    let (code, _, stderr) = child.finish_open(EXIT_WATCHDOG).await;

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
            let (code, _, _) = feed_and_wait(spawn_agent(&arg_refs, &[]), input).await;
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

        let (code, _, _) = feed_and_wait(spawn_agent(&arg_refs, &[]), input).await;

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
    let (code, stdout, _) = running.finish_open(EXIT_WATCHDOG).await;

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
        spawn_agent(
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
        spawn_agent(
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
        result.contains("absolute path") && result.contains("/etc/passwd"),
        "the model sees the refusal: {result}"
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
        spawn_agent(
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
        spawn_agent(
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
            assert_eq!(*source, agent_core::InputSource::User)
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
        spawn_agent(
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
        spawn_agent(
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
    let ws_a = sessions.join("ws-a");
    let ws_b = sessions.join("ws-b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let server = MockServer::start(vec![text_reply("a"), text_reply("b")]).await;

    for (id, workspace) in [("a-1", &ws_a), ("b-1", &ws_b)] {
        let (code, _, stderr) = feed_and_wait(
            spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
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
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--session-id",
                "s-secret",
                "--session-dir",
                sessions.to_str().unwrap(),
                "--api-key-env",
                "AGENT_CLI_FILE_KEY",
                "--header",
                "x-team-token=file-secret-value",
                "-p",
                "hi",
            ],
            &[("AGENT_CLI_FILE_KEY", KEY)],
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
    let server = MockServer::start(vec![
        tool_reply("call_1", "fx__sleep", "{\"ms\": 30000}"),
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
    // it is durably claimed (a second InputClaimed) before killing.
    first.write("steer-line\n").await;
    wait_until("the steer to be claimed", || {
        std::fs::read_to_string(&path)
            .map(|text| text.matches("\"InputClaimed\"").count() >= 2)
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
        spawn_agent(
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
    let texts: Vec<String> = history.iter().map(agent_core::Message::text).collect();
    assert!(
        texts.contains(&"steer-line".to_owned()),
        "the claimed steer was lost: {texts:?}"
    );
    let repaired = history.iter().find_map(|m| match m {
        agent_core::Message::Tool { result, .. } if result.call_id == "call_1" => Some(result),
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

/// 1: `full-access` is never the default, and without `--workspace` no write/execute tool
/// exists at all.
#[tokio::test]
async fn full_access_is_not_the_default_and_no_write_tools_without_a_workspace() {
    let server = MockServer::start(vec![
        tool_reply("c1", "write", r#"{"path":"x.txt","content":"y"}"#),
        tool_reply("c2", "shell", r#"{"command":"echo hi"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, stdout, stderr) = feed_and_wait(
        spawn_agent(
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
    // The default is named on startup, and it is not full-access.
    assert!(stderr.contains("[profile] workspace-write"), "{stderr}");
    assert!(!stderr.contains("full-access (chosen"), "{stderr}");
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
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
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
/// (ADR 0012 decision 4).
#[tokio::test]
async fn the_startup_text_does_not_claim_credential_isolation() {
    let dir = scratch("wording");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![text_reply("done")]).await;

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
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
    assert!(stderr.contains("[profile] workspace-write"), "{stderr}");
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
    // ADR 0012, decision 2026-10-12 (option B): MCP servers keep running unsandboxed
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

/// 3: a shell command asks; "allow once" runs it; the *next* identical command asks again.
#[tokio::test]
async fn a_shell_command_asks_every_time_when_allowed_once() {
    let dir = scratch("shell-ask");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        shell_reply("c1", "echo hi"),
        shell_reply("c2", "echo hi"),
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
/// no grant and asks again.
#[tokio::test]
async fn allow_for_session_covers_the_turn_and_not_a_resume() {
    let sessions = fresh_session_dir();
    let dir = scratch("shell-session");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let two_calls = Reply::sse(&[
        chunk(
            json!({"tool_calls": [
                {"index": 0, "id": "c1", "type": "function",
                 "function": {"name": "shell", "arguments": "{\"command\":\"echo hi\"}"}},
                {"index": 1, "id": "c2", "type": "function",
                 "function": {"name": "shell", "arguments": "{\"command\":\"echo hi\"}"}}
            ]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let server = MockServer::start(vec![
        two_calls,
        text_reply("done"),
        shell_reply("c3", "echo hi"),
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

/// 5: an allow rule is matched at the subcommand level, so a composite command always asks.
#[tokio::test]
async fn a_composite_shell_command_always_asks() {
    let dir = scratch("composite");
    let ws = dir.join("ws");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let sentinel = outside.join("escaped.txt");
    let composite = format!("git status; touch {}", sentinel.display());
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
            "--allow-shell-command",
            "git status",
        ],
        &[],
    );
    child.write("go\n").await;
    // The simple command is allowed by the rule; only the composite asks.
    child.wait_for_stderr_count("[y/N]", 1).await;
    child.write("n\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(
        stderr.matches("[y/N]").count(),
        1,
        "only the composite may ask: {stderr}"
    );
    assert!(
        !sentinel.exists(),
        "a composite command must never be let through by an allow rule"
    );
    let simple = tool_result_for(&server, 1, "c1");
    assert!(
        simple.contains("$ git status") && !simple.contains("denied"),
        "the allowed simple command ran: {simple}"
    );
    let composite = tool_result_for(&server, 2, "c2");
    assert!(
        composite.contains("denied"),
        "the composite was denied: {composite}"
    );
}

/// 6: `--yes` removes the question; it does not override a `deny` rule.
#[tokio::test]
async fn yes_does_not_bypass_a_deny_rule() {
    let dir = scratch("deny");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![
        tool_reply("c1", "write", r#"{"path":"new.txt","content":"x"}"#),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--profile",
                "safe",
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
    assert!(
        result.contains("denied") || result.contains("safe mode does not modify files"),
        "{result}"
    );
    assert!(
        !ws.join("new.txt").exists(),
        "--yes must not write when a rule denies"
    );
}

/// 7: Ctrl-C while waiting for a shell approval cancels the turn; the process does not hang.
#[tokio::test]
async fn cancelling_at_the_shell_approval_does_not_hang() {
    let dir = scratch("shell-cancel");
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let server = MockServer::start(vec![shell_reply("c1", "echo hi"), text_reply("done")]).await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
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
                 "function": {"name": "shell", "arguments": "{\"command\":\"echo hi\"}"}},
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
            "--session-id",
            "s-audit",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("go\n").await;
    // `read` is allowed by a rule (no prompt); `shell` asks; `write` outside the workspace
    // has no rule and falls to ask.
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

/// 9: under `workspace-write`, a shell command cannot write outside the workspace.
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
        spawn_agent(
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
    let result = tool_result_content(&server, 1);
    assert!(
        result.contains("exit code: 1"),
        "the sandboxed command must fail: {result}"
    );
    assert!(
        !target.exists(),
        "the sandbox let a write escape the workspace"
    );
}

// ---------- context budget (ADR 0015) ----------

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

fn budget_workspace(label: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(label);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (dir, ws)
}

/// 1: without `--context-window` the guard is off, the startup says so, and no stop happens.
#[tokio::test]
async fn without_a_window_the_guard_is_off_and_says_so() {
    let (_dir, ws) = budget_workspace("budget-off");
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 3300),
        text_reply("done"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
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
    assert!(stderr.contains("context guard off"), "{stderr}");
    assert!(stderr.contains("no --context-window"), "{stderr}");
    assert!(
        !stderr.contains("the model request was not sent"),
        "{stderr}"
    );
    // The turn ran to completion: the second request was sent.
    assert_eq!(server.requests().len(), 2, "{stderr}");
}

/// 2: a reported input near the threshold blocks the next request before it is sent.
#[tokio::test]
async fn a_reported_usage_near_the_threshold_stops_before_the_next_request() {
    let (_dir, ws) = budget_workspace("budget-stop");
    let server = MockServer::start(vec![used_tool_reply(
        "c1",
        "write",
        r#"{"path":"f.txt","content":"x"}"#,
        3300,
    )])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
            "--context-window",
            "4000",
        ],
        &[],
    );
    child.write("go\n").await;
    child
        .wait_for_stderr("the model request was not sent")
        .await;
    assert_eq!(
        server.requests().len(),
        1,
        "the over-budget request must never reach the server"
    );
    child.write("n\n").await; // stop
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("window 4000 tokens, threshold 80%"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), 1);
}

/// 3: the stop text names its source: reported + estimated shows both parts, a pure estimate
/// is marked as one, and they are different strings.
#[tokio::test]
async fn the_stop_text_distinguishes_reported_from_estimated() {
    // (a) a reported baseline plus estimated growth.
    let (_dir, ws) = budget_workspace("budget-reported");
    let server = MockServer::start(vec![used_tool_reply(
        "c1",
        "write",
        r#"{"path":"f.txt","content":"x"}"#,
        3300,
    )])
    .await;
    let (code, _, reported) = feed_and_wait(
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "4000",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;
    assert_eq!(code, 3, "{reported}");
    assert!(
        reported.contains("(reported 3300 + estimated "),
        "the reported baseline and the estimated growth must be shown separately: {reported}"
    );

    // (b) nothing was reported: the whole projection is an estimate.
    let (_dir2, ws2) = budget_workspace("budget-estimated");
    std::fs::write(ws2.join("big.txt"), "x".repeat(64 * 1024)).unwrap();
    let server = MockServer::start(vec![tool_reply("c1", "read", r#"{"path":"big.txt"}"#)]).await;
    let (code, _, estimated) = feed_and_wait(
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws2.to_str().unwrap(),
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
    assert_eq!(code, 3, "{estimated}");
    assert!(
        estimated.contains("~") && estimated.contains("(estimated)"),
        "a pure estimate must be marked as one: {estimated}"
    );
    assert!(
        !estimated.contains("(reported"),
        "an estimate must never be displayed as a report: {estimated}"
    );
    assert_ne!(reported, estimated);
}

/// 4: choosing continue lets the next request through once; the request after it is checked
/// again (the guard is not switched off).
#[tokio::test]
async fn continue_relaxes_once_and_the_guard_comes_back() {
    let (_dir, ws) = budget_workspace("budget-continue");
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 3300),
        used_tool_reply("c2", "write", r#"{"path":"g.txt","content":"y"}"#, 3300),
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
            "--yes",
            "--context-window",
            "4000",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[c] continue", 1).await;
    assert_eq!(server.requests().len(), 1);
    child.write("c\n").await;
    // The relaxed request is sent; its tool result pushes the next one over the line again.
    child.wait_for_stderr_count("[c] continue", 2).await;
    assert_eq!(
        server.requests().len(),
        2,
        "the relaxed request was sent, and the one after it was blocked"
    );
    child.write("n\n").await;
    // A later turn is still guarded: the relax was for one request, not for the session.
    child.write("hi\n").await;
    child.wait_for_stderr_count("[c] continue", 3).await;
    assert_eq!(
        server.requests().len(),
        2,
        "a later turn must not be relaxed by an earlier continue"
    );
    child.write("n\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(server.requests().len(), 2, "{stderr}");
    assert_eq!(stderr.matches("[c] continue").count(), 3, "{stderr}");
}

/// 5: choosing stop ends the turn; the session keeps accepting turns.
#[tokio::test]
async fn stopping_leaves_the_session_usable() {
    let (_dir, ws) = budget_workspace("budget-stop-session");
    let server = MockServer::start(vec![used_tool_reply(
        "c1",
        "write",
        r#"{"path":"f.txt","content":"x"}"#,
        3300,
    )])
    .await;

    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
            "--context-window",
            "4000",
        ],
        &[],
    );
    child.write("go\n").await;
    child.wait_for_stderr_count("[c] continue", 1).await;
    child.write("n\n").await; // give up this turn
    // The prompt is back: another turn is accepted (and stops again, because the guard is
    // still on).
    child.write("hi\n").await;
    child.wait_for_stderr_count("[c] continue", 2).await;
    child.write("n\n").await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;

    assert_eq!(code, 0, "{stderr}");
    assert_eq!(stderr.matches("[c] continue").count(), 2, "{stderr}");
}

/// 6: one-shot mode never continues by itself; it ends with its own exit code.
#[tokio::test]
async fn one_shot_stops_with_its_own_exit_code() {
    let (_dir, ws) = budget_workspace("budget-oneshot");
    let server = MockServer::start(vec![used_tool_reply(
        "c1",
        "write",
        r#"{"path":"f.txt","content":"x"}"#,
        3300,
    )])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "4000",
                "-p",
                "go",
            ],
            &[],
        ),
        "",
    )
    .await;

    assert_eq!(code, 3, "{stderr}");
    assert!(
        stderr.contains("the model request was not sent"),
        "{stderr}"
    );
    assert!(
        stderr.contains("does not continue automatically"),
        "{stderr}"
    );
    assert_eq!(server.requests().len(), 1, "nothing was sent automatically");
}

/// 7: the stop is durable (`TurnFinished` = `context_budget` plus `ContextBudgetReached`), and
/// a resumed session still has the history.
///
/// Note what resume does *not* restore: the reported-input baseline is in-memory only
/// (`AgentSession::budget_baseline`), so after `--resume` the projection is an estimate. That
/// is why this run uses a wide window: the resumed request must go out so the recovered
/// history is observable in it.
#[tokio::test]
async fn the_stop_is_durable_and_a_resume_keeps_the_history() {
    let sessions = fresh_session_dir();
    let (_dir, ws) = budget_workspace("budget-durable");
    let server = MockServer::start(vec![
        used_tool_reply("c1", "write", r#"{"path":"f.txt","content":"x"}"#, 3300),
        text_reply("after resume"),
    ])
    .await;

    let (code, _, stderr) = feed_and_wait(
        spawn_agent(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                ws.to_str().unwrap(),
                "--yes",
                "--context-window",
                "4000",
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
    assert_eq!(code, 3, "{stderr}");

    let raw = std::fs::read_to_string(sessions.join("s-budget.jsonl")).unwrap();
    assert!(raw.contains("\"context_budget\""), "{raw}");
    assert!(raw.contains("ContextBudgetReached"), "{raw}");

    // Resume: the recovered history is carried into the next request.
    let mut child = Running::spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--workspace",
            ws.to_str().unwrap(),
            "--yes",
            "--context-window",
            "100000",
            "--resume",
            "s-budget",
            "--session-dir",
            sessions.to_str().unwrap(),
        ],
        &[],
    );
    child.write("again\n").await;
    wait_until("the resumed request", || server.requests().len() == 2).await;
    child.write("/exit\n").await;
    let (code, _, stderr) = child.finish().await;
    assert_eq!(code, 0, "{stderr}");

    let resumed = messages_of(&server, 1);
    assert_eq!(last_user_text(&resumed), "again");
    let texts: Vec<&str> = resumed
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect();
    assert!(
        texts.contains(&"go"),
        "the resumed request lacks the old input: {resumed}"
    );
}

/// Like [`spawn_agent`], but uses a caller-owned session directory, so a test can assert
/// that a failed start left it empty (dogfooding D2).
fn spawn_agent_in(session_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Child {
    Command::new(agent_bin())
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env("RADIAN_SESSION_DIR", session_dir)
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
        spawn_agent_in(
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
        !stderr.contains("full-access"),
        "a bad workspace must not suggest full-access: {stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
    assert_session_dir_empty(&session_dir);
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
        spawn_agent_in(
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
        spawn_agent_in(
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
        spawn_agent_in(
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
        spawn_agent(
            &[
                "--model",
                "claude-x",
                "--provider",
                "anthropic",
                "--base-url",
                &server.base_url,
                "--api-key-env",
                "ANTHROPIC_API_KEY",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-test-key")],
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
    // Default thinking is off: the request has no `thinking` key at all.
    assert!(body.get("thinking").is_none(), "{body}");
}

/// Without `--api-key-env`, `--provider anthropic` reads `ANTHROPIC_API_KEY`; the key never
/// appears in the output or the session file.
#[tokio::test]
async fn anthropic_reads_anthropic_api_key_env_by_default_and_never_leaks_it() {
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    let dir = fresh_session_dir();
    let (code, stdout, stderr) = feed_and_wait(
        spawn_agent_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--base-url",
                &server.base_url,
                "--session-id",
                "anthro-env",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-default-env-key")],
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

/// `--thinking adaptive` puts `{"type":"adaptive"}` in the request body; `enabled:<budget>`
/// puts the budget in. The default (no `--thinking`) sends no `thinking` key, asserted in the
/// messages test above.
#[tokio::test]
async fn anthropic_thinking_modes_are_sent_in_the_body() {
    for (mode, expected) in [
        ("adaptive", json!({"type": "adaptive"})),
        (
            "enabled:8192",
            json!({"type": "enabled", "budget_tokens": 8192}),
        ),
    ] {
        let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
        let (code, _, stderr) = feed_and_wait(
            spawn_agent(
                &[
                    "--model",
                    "claude",
                    "--provider",
                    "anthropic",
                    "--base-url",
                    &server.base_url,
                    "--thinking",
                    mode,
                    "-p",
                    "hi",
                ],
                &[("ANTHROPIC_API_KEY", "sk-ant-thinking")],
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
        spawn_agent_in(
            &dir,
            &[
                "--model",
                "claude",
                "--provider",
                "anthropic",
                "--base-url",
                &server.base_url,
                "--session-id",
                "snap-ant",
                "-p",
                "hi",
            ],
            &[("ANTHROPIC_API_KEY", "sk-ant-snap")],
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
        spawn_agent_in(
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
        spawn_agent(
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
async fn anthropic_rejects_a_thinking_budget_out_of_range_before_creating_a_session() {
    let server = anth::MockServer::start(vec![anth_text_reply("ok")]).await;
    for (mode, budget) in [("enabled:16384", 16384u64), ("enabled:1000", 1000)] {
        let dir = fresh_session_dir();
        let (code, _, stderr) = feed_and_wait(
            spawn_agent_in(
                &dir,
                &[
                    "--model",
                    "claude",
                    "--provider",
                    "anthropic",
                    "--base-url",
                    &server.base_url,
                    "--thinking",
                    mode,
                    "-p",
                    "hi",
                ],
                &[("ANTHROPIC_API_KEY", "sk-ant-budget")],
            ),
            "",
        )
        .await;
        assert_eq!(code, 2, "{mode} must be a usage error: {stderr}");
        assert!(stderr.contains("out of range"), "{mode}: {stderr}");
        assert!(
            stderr.contains(&format!("enabled:{budget}")),
            "the message must name the budget: {stderr}"
        );
        // The rule and both numbers are in the message.
        assert!(stderr.contains("1024"), "{mode}: {stderr}");
        assert!(stderr.contains("16384"), "{mode}: {stderr}");
        // No session file was created, and no request was sent.
        assert_session_dir_empty(&dir);
    }
    assert!(
        server.requests().is_empty(),
        "an out-of-range budget must not reach the model"
    );
}
