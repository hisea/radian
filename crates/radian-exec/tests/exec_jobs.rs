//! `radian exec -p` waits for its background jobs (ADR 0037 decision 3, ADR 0044): a turn
//! that starts a background job does not end the run; each settle wakes a turn through the
//! notifier, and `exec` exits with the last turn's outcome once no job is running and no
//! woken turn is left. A closed wake gate ends the run with exit 3.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion (the same shape as `cli.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

/// Whether a pid is still alive (`kill 0` only probes).
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 checks existence and sends nothing.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Waits for one pid to disappear, reporting whether it did within `limit`.
#[cfg(unix)]
async fn gone_within(pid: u32, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Waits for a hook to record its pid while `exec` still runs.
#[cfg(unix)]
async fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the async hook never recorded its pid in {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A fresh directory that no test shares, so nothing touches the user's real HOME, config
/// or credentials.
fn scratch(prefix: &str) -> PathBuf {
    common::process_dir_in(
        Path::new("/tmp"),
        &format!("radian-exec-exec-jobs-{prefix}"),
    )
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

/// Spawns `radian exec` in `env` with real built-in tools (`--workspace`, `--yes`) and
/// [`common::fast_catalog_url`], so no run reads the user's state or reaches the
/// network.
fn spawn(args: &[&str], env: &Env) -> Child {
    let mut cmd = Command::new(common::radian_bin());
    cmd.arg("exec")
        .args(args)
        .env("HOME", &env.home)
        .env("RADIAN_HOME", &env.home)
        .env("RADIAN_SESSION_DIR", &env.sessions)
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
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
///
/// A settle that lands while the turn still runs takes the busy lane
/// (`inject_pending_wake`: `wake: false`, `pending_wake: true`). Absorbed at a step
/// boundary it opens no turn; still queued at the turn boundary it opens the next turn
/// itself and counts as one wake (ADR 0011 L5, ADR 0037 decision 5). So the count
/// resolves each later turn's opening claim back to its enqueue instead of filtering
/// on `"wake": true` alone: `"wake": true` misses a stranded notice (two turns, zero
/// wakes) and counting every `pending_wake` enqueue would charge an absorbed one.
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
        let journal_text =
            std::fs::read_to_string(env.sessions.join(format!("{sid}.jsonl"))).unwrap();
        // Every later turn must open on a System wake: an idle `followup`
        // (`wake: true`) or a notice stranded past the turn end (`pending_wake:
        // true`, `wake: false`), which opens its turn itself and counts as one
        // wake (ADR 0011 L5, ADR 0037 decision 5). The opener is the first
        // `InputClaimed` for each later turn, resolved back to its enqueue; an
        // absorbed notice is claimed mid-turn and never counts.
        let mut enqueued: std::collections::HashMap<String, (String, bool, bool)> =
            std::collections::HashMap::new();
        let mut first_claim: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut turn_order: Vec<String> = Vec::new();
        for line in journal_text.lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            match record.get("event").and_then(Value::as_str) {
                Some("InboxEnqueued") => {
                    let data = &record["data"];
                    enqueued.insert(
                        data["message_id"].as_str().unwrap().to_owned(),
                        (
                            data["source"].as_str().unwrap().to_owned(),
                            data["wake"].as_bool().unwrap(),
                            data.get("pending_wake")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        ),
                    );
                }
                Some("InputClaimed") => {
                    let data = &record["data"];
                    first_claim
                        .entry(data["turn_id"].as_str().unwrap().to_owned())
                        .or_insert_with(|| {
                            data["message_ids"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|id| id.as_str().unwrap().to_owned())
                                .collect()
                        });
                }
                Some("TurnStarted") => {
                    turn_order.push(record["data"]["turn_id"].as_str().unwrap().to_owned());
                }
                _ => {}
            }
        }
        let sys_wakes = enqueued
            .values()
            .filter(|(source, wake, _)| source == "system" && *wake)
            .count();
        let mut stranded = 0;
        for turn_id in turn_order.iter().skip(1) {
            let opener = first_claim
                .get(turn_id)
                .unwrap_or_else(|| panic!("iteration {i}: turn {turn_id} has no claimed input"));
            assert!(
                !opener.is_empty(),
                "iteration {i}: turn {turn_id} claimed no input"
            );
            for id in opener {
                let (source, wake, pending_wake) = enqueued.get(id).unwrap_or_else(|| {
                    panic!("iteration {i}: claimed input {id} was never enqueued")
                });
                assert!(
                    source == "system" && (*wake || *pending_wake),
                    "iteration {i}: turn {turn_id} opened on a non-wake input {id} ({source})"
                );
                if *pending_wake && !*wake {
                    stranded += 1;
                }
            }
        }
        woken_total += sys_wakes + stranded;
        assert_eq!(
            turn_order.len(),
            1 + sys_wakes + stranded,
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
/// the `Result` record. Every continued turn calls a tool, so it does real work: the
/// consecutive idle-continuation cap never fires and the window is the cap under test.
/// Mutation: exec ignores a degraded Stop (exits 0 with the verdict unread) → this fails.
#[tokio::test]
async fn p_exits_three_when_a_stop_hook_fills_the_wake_gate() {
    let mut replies = vec![text_reply("first")];
    for i in 1..=10 {
        // A tool call per continued turn: the turn's second model call ends it, and
        // `model_calls > 1` resets the idle-continuation streak (ADR 0045 §3).
        replies.push(tool_reply(
            &format!("call-{i}"),
            "shell",
            &json!({"command": "true"}).to_string(),
        ));
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

/// The consecutive idle hook-continuation cap ends a `-p` run the same way a spent wake
/// budget does: the continuation a hook asks for after seven idle continued turns degrades
/// to inject-only, and no turn will ever meet it, so exec ends with the budget ending. This
/// is the cap, not the window: only seven wakes were used, and eight turns ran (the prompt's
/// turn plus seven continuations).
/// Mutation: the cap stops counting as unhandled (`degraded`) → exec exits 0 with the
/// verdict unread, and this fails.
#[tokio::test]
async fn p_ends_on_the_idle_hook_continuation_cap() {
    let mut replies = vec![text_reply("first")];
    for i in 1..=7 {
        replies.push(text_reply(&format!("again {i}")));
    }
    let server = MockServer::start(replies).await;
    let env = Env::fresh("stop-idle-cap");
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
        "stop-idle-cap",
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
        "the idle cap shares the wake-budget ending (docs/user/cli.md): {result}"
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
    assert_eq!(
        result.get("turns"),
        Some(&json!(8)),
        "the prompt's turn plus seven continuations; the eighth does not open: {result}"
    );
    assert_eq!(
        result.get("text"),
        Some(&json!("again 7")),
        "the last turn only talked: {result}"
    );
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

/// Edge case 5: `exec -p` does not wait for an async hook (ADR 0045 §5). A `PostToolUse`
/// hook that sleeps 30 s is killed when the run ends: the process exits promptly and the
/// hook's process group is gone. Without the session-end kill the run waits out the sleep
/// (30 s, past the 20 s assertion) and the pid stays alive; an end that waits for the
/// blocking reader would hang for the same 30 s.
#[cfg(unix)]
#[tokio::test]
async fn p_exits_promptly_and_kills_a_running_async_hook() {
    let server = MockServer::start(vec![
        tool_reply("c1", "shell", &json!({"command": "true"}).to_string()),
        text_reply("done"),
    ])
    .await;
    let env = Env::fresh("async-hook-exit");
    let pidfile = env.ws.join("hook.pid");
    let config = env.home.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "schema_version = 1\n[model]\nmodel = \"m\"\n\
             [[hooks.PostToolUse]]\nmatcher = \"shell\"\n[[hooks.PostToolUse.hooks]]\n\
             type = \"command\"\ncommand = \"echo $$ > {}; sleep 30\"\nasync = true\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let args = [
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        env.ws.to_str().unwrap(),
        "--yes",
        "--session-id",
        "async-hook-exit",
        "--config",
        config.to_str().unwrap(),
        "-p",
        "hello",
    ];
    let started = std::time::Instant::now();
    let mut child = spawn(&args, &env);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"")
        .await
        .expect("stdin");
    let pid = wait_for_pid_file(&pidfile).await;
    let output = timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("exec did not exit within the watchdog")
        .expect("wait");
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(0), "{:?}", output);
    assert!(
        elapsed < Duration::from_secs(20),
        "exec waited for the async hook: {elapsed:?}"
    );
    assert!(
        gone_within(pid, Duration::from_secs(10)).await,
        "the async hook's process group survived exec (pid {pid})"
    );
}
