//! `radian exec -p` waits for its background jobs (ADR 0037 decision 3, ADR 0044): a turn
//! that starts a background job does not end the run; each settle wakes a turn through the
//! notifier, and `exec` exits with the last turn's outcome once no job is running and no
//! woken turn is left. A closed wake gate ends the run with exit 3.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion (the same shape as `cli.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

/// A fresh directory that no test shares, so nothing touches the user's real HOME, config
/// or credentials.
fn scratch(prefix: &str) -> PathBuf {
    static DIRS: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-cli-exec-jobs-{prefix}-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One isolated environment: a HOME (also `RADIAN_HOME`), a session directory the test can
/// read back, and a workspace real shell jobs run in.
struct Env {
    home: PathBuf,
    sessions: PathBuf,
    ws: PathBuf,
}

impl Env {
    fn fresh(prefix: &str) -> Self {
        let home = scratch(&format!("{prefix}-home"));
        let sessions = scratch(&format!("{prefix}-sessions"));
        let ws = scratch(&format!("{prefix}-ws"));
        Self { home, sessions, ws }
    }
}

/// Spawns `radian exec` in `env` with real built-in tools (`--workspace`, `--yes`) and a
/// catalog URL pointing at a closed port, so no run reads the user's state or reaches the
/// network.
fn spawn(args: &[&str], env: &Env) -> Child {
    let mut cmd = Command::new(common::radian_bin());
    cmd.arg("exec")
        .args(args)
        .env("HOME", &env.home)
        .env("RADIAN_HOME", &env.home)
        .env("RADIAN_SESSION_DIR", &env.sessions)
        .env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json")
        .env_remove("RADIAN_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn().expect("spawn radian")
}

/// Closes stdin and waits with a cap so a stuck process fails the test instead of hanging
/// it.
async fn feed(mut child: Child, input: &str) -> (i32, String, String) {
    let _ = child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .await;
    let output = timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("the process did not exit within the watchdog")
        .expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
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

/// One real background job: the shell tool with `background: true`.
fn bg_reply(id: &str, command: &str) -> Reply {
    tool_reply(
        id,
        "shell",
        &json!({"command": command, "background": true}).to_string(),
    )
}

/// Runs one `-p` turn in `--output jsonl` mode and returns the exit code, the closing
/// `Result` record, and stderr. `extra` prepends CLI flags (e.g. `--config`).
async fn run_jsonl(
    server_base_url: &str,
    env: &Env,
    session_id: &str,
    prompt: &str,
    extra: &[&str],
) -> (i32, Value, String) {
    let mut args: Vec<&str> = vec![
        "--model",
        "m",
        "--base-url",
        server_base_url,
        "--workspace",
        env.ws.to_str().unwrap(),
        "--yes",
        "--session-id",
        session_id,
        "--output",
        "jsonl",
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["-p", prompt]);
    let (code, out, err) = feed(spawn(&args, env), "").await;
    let mut lines: Vec<&str> = out.lines().collect();
    assert!(!lines.is_empty(), "jsonl stdout ends with a Result line");
    let result: Value =
        serde_json::from_str(lines.pop().unwrap()).expect("the last line is the Result record");
    (code, result, err)
}

/// `-p` whose first turn starts a background job and ends: exec waits, the settle wakes a
/// second turn, and exec exits 0 with that turn's `Result`.
/// Mutation: exit right after the first turn → `turns` stays 1 and this fails.
#[tokio::test]
async fn p_waits_for_a_background_job_and_exits_with_the_woken_turn() {
    let server = MockServer::start(vec![
        bg_reply("c1", "sleep 3"),
        text_reply("first"),
        text_reply("second"),
    ])
    .await;
    let env = Env::fresh("basic");
    let (code, result, err) = run_jsonl(&server.base_url, &env, "basic", "start a job", &[]).await;

    assert_eq!(code, 0, "{err}");
    assert_eq!(result.get("outcome"), Some(&json!("completed")), "{result}");
    assert_eq!(result.get("turns"), Some(&json!(2)), "{result}");
    assert_eq!(result.get("text"), Some(&json!("second")), "{result}");
}

/// Two jobs settling one after the other wake two turns, then exec exits.
/// Mutation: exit right after the first turn → `turns` stays 1 and this fails.
#[tokio::test]
async fn p_chains_two_jobs_through_two_woken_turns() {
    let server = MockServer::start(vec![
        bg_reply("c1", "sleep 2"),
        text_reply("first"),
        bg_reply("c2", "sleep 2"),
        text_reply("got A"),
        text_reply("got B"),
    ])
    .await;
    let env = Env::fresh("chain");
    let (code, out, err) = feed(
        spawn(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--yes",
                "--session-id",
                "chain",
                "-p",
                "start two jobs in sequence",
            ],
            &env,
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{err}");
    let first = out.find("first").expect("the first turn streamed: {out}");
    let got_a = out.find("got A").expect("the second turn streamed: {out}");
    let got_b = out.find("got B").expect("the third turn streamed: {out}");
    assert!(
        first < got_a && got_a < got_b,
        "turns stream in order: {out}"
    );
}

/// A job settling at the same moment the turn ends is still waited for: wherever the
/// settle lands (absorbed into the ending turn, or waking a new one), every wake notice
/// runs its turn. Twenty iterations with staggered sleeps cover the interleavings; the
/// journal invariant holds for each one.
/// Mutation: drop the handled-settles check → iterations whose settle lands between the
/// registry and the inbox exit before the woken turn runs, and the invariant fails.
#[tokio::test]
async fn p_waits_for_a_job_settling_as_the_turn_ends() {
    let mut woken_total = 0;
    for i in 0..20 {
        // 0.1..0.5 s against a ~0.2 s first turn: settles land before, during, and
        // after the turn end across iterations.
        let sleep = format!("sleep 0.{}", 1 + (i % 5));
        let server = MockServer::start(vec![
            bg_reply("c1", &sleep),
            text_reply("first"),
            text_reply("second"),
        ])
        .await;
        let env = Env::fresh(&format!("race-{i}"));
        let sid = format!("race-{i}");
        let (code, out, err) = feed(
            spawn(
                &[
                    "--model",
                    "m",
                    "--base-url",
                    &server.base_url,
                    "--workspace",
                    env.ws.to_str().unwrap(),
                    "--yes",
                    "--session-id",
                    &sid,
                    "-p",
                    "start a tiny job",
                ],
                &env,
            ),
            "",
        )
        .await;

        assert_eq!(code, 0, "iteration {i}: {err}");
        assert!(out.contains("first"), "iteration {i}: {out}");
        let journal = std::fs::read_to_string(env.sessions.join(format!("{sid}.jsonl"))).unwrap();
        let turns_started = journal.matches("\"event\":\"TurnStarted\"").count();
        let sys_wakes = journal
            .lines()
            .filter(|line| {
                line.contains("\"event\":\"InboxEnqueued\"")
                    && line.contains("\"source\":\"system\"")
                    && line.contains("\"wake\":true")
            })
            .count();
        woken_total += sys_wakes;
        assert_eq!(
            turns_started,
            1 + sys_wakes,
            "iteration {i}: every wake notice ran its turn"
        );
    }
    assert!(
        woken_total >= 3,
        "the loop must exercise the woken-turn path, not only absorbs: {woken_total}"
    );
}

/// Eleven settles inside one window fill the rate gate (10 wakes per 10 minutes): each
/// woken turn starts the next job, the first ten settles wake a turn each, and the 11th
/// notice is degraded to inject-only — so exec ends the session with exit 3 and names
/// the unhandled notice in the `Result` record. (Eleven jobs at once is impossible: at
/// most 4 background jobs run concurrently, ADR 0037 decision 9.)
/// Mutation: exit 0 instead of 3 on a closed gate → this fails.
#[tokio::test]
async fn p_exits_three_when_the_wake_gate_closes() {
    let mut replies = vec![bg_reply("c1", "sleep 2"), text_reply("first")];
    for i in 2..=11 {
        // Each woken turn starts the next 2-second job and ends: every settle lands
        // while idle, each waking exactly one turn until the gate is full.
        replies.push(bg_reply(&format!("c{i}"), "sleep 2"));
        replies.push(text_reply(&format!("got {}", i - 1)));
    }
    let server = MockServer::start(replies).await;
    let env = Env::fresh("gate");
    let (code, result, err) =
        run_jsonl(&server.base_url, &env, "gate", "start eleven jobs", &[]).await;

    assert_eq!(code, 3, "{err}");
    assert_eq!(
        result.get("outcome"),
        Some(&json!("budget_exceeded")),
        "{result}"
    );
    assert_eq!(result.get("exit_code"), Some(&json!(3)), "{result}");
    assert_eq!(
        result.get("stop_reason"),
        Some(&json!("wake_budget")),
        "{result}"
    );
    assert_eq!(
        result.get("unhandled_job_notices"),
        Some(&json!(1)),
        "{result}"
    );
    assert_eq!(result.get("turns"), Some(&json!(11)), "{result}");
    assert!(
        err.contains("wake budget is used up"),
        "the degraded ending is visible on stderr: {err}"
    );
}

/// A `Stop` hook that always asks for another turn fills the rate gate the same way job
/// wakes do: past ten continuations its verdict degrades to inject-only, so exec ends
/// the session with exit 3 and names the unhandled Stop verdict (not a job notice) in
/// the `Result` record.
/// Mutation: exec ignores a degraded Stop (exits 0 with the verdict unread) → this fails.
#[tokio::test]
async fn p_exits_three_when_a_stop_hook_fills_the_wake_gate() {
    let mut replies = vec![text_reply("first")];
    for i in 1..=10 {
        replies.push(text_reply(&format!("again {i}")));
    }
    let server = MockServer::start(replies).await;
    let env = Env::fresh("stop-gate");
    let config = env.home.join("config.toml");
    std::fs::write(
        &config,
        "schema_version = 1\n[model]\nmodel = \"m\"\n\
         [[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"exit 2\"\n",
    )
    .unwrap();
    let (code, result, err) = run_jsonl(
        &server.base_url,
        &env,
        "stop-gate",
        "keep going",
        &["--config", config.to_str().unwrap()],
    )
    .await;

    assert_eq!(code, 3, "{err}");
    assert_eq!(
        result.get("outcome"),
        Some(&json!("budget_exceeded")),
        "{result}"
    );
    assert_eq!(result.get("exit_code"), Some(&json!(3)), "{result}");
    assert_eq!(
        result.get("stop_reason"),
        Some(&json!("wake_budget")),
        "{result}"
    );
    assert_eq!(
        result.get("unhandled_job_notices"),
        Some(&json!(0)),
        "no job notice waits: {result}"
    );
    assert_eq!(
        result.get("unhandled_stop_notices"),
        Some(&json!(1)),
        "{result}"
    );
    assert_eq!(result.get("turns"), Some(&json!(11)), "{result}");
    assert!(
        err.contains("wake budget is used up"),
        "the degraded ending is visible on stderr: {err}"
    );
}

/// No background job: the wait loop breaks at once, so default stdout and the exit are
/// exactly what they were before (byte-identical single text line, exit 0).
#[tokio::test]
async fn p_without_a_background_job_is_unchanged() {
    let server = MockServer::start(vec![text_reply("hello")]).await;
    let env = Env::fresh("nojob");
    let (code, out, err) = feed(
        spawn(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--workspace",
                env.ws.to_str().unwrap(),
                "--yes",
                "-p",
                "hi",
            ],
            &env,
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "hello\n", "stdout carries only the model text");
}
