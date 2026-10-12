//! The `agent` CLI against a mock model server and the real MCP fixture, as subprocesses.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

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

fn spawn_agent(args: &[&str], envs: &[(&str, &str)]) -> Child {
    Command::new(agent_bin())
        .args(args)
        .env_remove("OPENAI_API_KEY")
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
    // waits out its full 2s cancel grace for a ping that can never be answered: a real,
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
    // Comfortably inside the 2s grace window the first signal opened.
    tokio::time::sleep(Duration::from_millis(300)).await;
    sigint(&child.child);

    let (code, _, _) = child.finish().await;

    assert_eq!(code, 130);
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
