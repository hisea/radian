//! `radian exec --output jsonl` and the outcome exit codes (ADR 0044): the golden
//! stdout shape, one test per exit code 0/1/3/4, `-p -` from stdin, and the default-mode
//! stdout pin.

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
    // /tmp, not $TMPDIR: on macOS the latter can push <home>/run/<pid>.sock past SUN_LEN (104).
    let dir = PathBuf::from("/tmp").join(format!(
        "radian-exec-jsonl-{prefix}-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One isolated environment: a HOME (also `RADIAN_HOME`) and an explicit session directory
/// the test names, so it can read the journal back.
struct Env {
    home: PathBuf,
    sessions: PathBuf,
}

impl Env {
    fn fresh(prefix: &str) -> Self {
        let home = scratch(&format!("{prefix}-home"));
        let sessions = scratch(&format!("{prefix}-sessions"));
        Self { home, sessions }
    }
}

/// Spawns `radian exec <args>` in `env`, plus [`common::fast_catalog_url`], so
/// no run reads the user's state or reaches the network.
fn spawn(args: &[&str], env: &Env) -> Child {
    Command::new(common::radian_bin())
        .arg("exec")
        .args(args)
        .env("HOME", &env.home)
        .env("RADIAN_HOME", &env.home)
        .env("RADIAN_SESSION_DIR", &env.sessions)
        .env("RADIAN_CATALOG_URL", common::fast_catalog_url())
        .env_remove("RADIAN_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn radian")
}

/// Writes all input, closes stdin, and waits with a cap so a stuck process fails the test.
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

/// A text answer that also reports provider usage for its request.
fn text_reply_with_usage(text: &str, input: u64, output: u64) -> Reply {
    Reply::sse(&[
        chunk(json!({"role": "assistant", "content": text}), None),
        chunk(json!({}), Some("stop")),
        json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}}),
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

/// Runs one `-p` turn with `--output jsonl` and splits stdout into its durable lines, its
/// live lines and its closing `Result` record.
async fn run_jsonl(
    server_base_url: &str,
    env: &Env,
    session_id: &str,
    extra_args: &[&str],
    prompt: &str,
    stdin: &str,
) -> (i32, Vec<String>, Vec<Value>, Value, String) {
    let mut args = vec![
        "exec".to_owned(),
        "--model".to_owned(),
        "m".to_owned(),
        "--base-url".to_owned(),
        server_base_url.to_owned(),
        "--session-id".to_owned(),
        session_id.to_owned(),
        "--output".to_owned(),
        "jsonl".to_owned(),
    ];
    args.extend(extra_args.iter().map(|arg| (*arg).to_owned()));
    args.push("-p".to_owned());
    args.push(prompt.to_owned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    // `spawn` adds its own `exec`; drop the first word.
    let (code, out, err) = feed(spawn(&arg_refs[1..], env), stdin).await;
    let mut lines: Vec<String> = out.lines().map(str::to_owned).collect();
    assert!(
        !lines.is_empty(),
        "jsonl stdout must end with a Result line"
    );
    let result: Value = serde_json::from_str(lines.pop().unwrap().as_str())
        .expect("the last stdout line is the Result record");
    assert_eq!(
        result.get("type"),
        Some(&Value::String("Result".to_owned())),
        "the last line is the Result record: {result}"
    );
    let mut durable = Vec::new();
    let mut live = Vec::new();
    for line in &lines {
        let value: Value =
            serde_json::from_str(line).expect("every stdout line is one JSON object");
        if value.get("live") == Some(&Value::Bool(true)) {
            live.push(value);
        } else {
            durable.push(line.clone());
        }
    }
    (code, durable, live, result, err)
}

/// The repository root (`crates/radian-exec` two levels down).
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("the crate lives in <root>/crates/radian-exec")
        .to_path_buf()
}

/// Default `-p` stdout is only the model text, byte for byte: no JSON line may leak into it.
/// Mutation: serialize a live line in default mode → this fails on the exact match.
#[tokio::test]
async fn default_p_stdout_is_only_model_text() {
    let server = MockServer::start(vec![text_reply("hello jsonl")]).await;
    let env = Env::fresh("default-stdout");
    let (code, out, err) = feed(
        spawn(
            &["--model", "m", "--base-url", &server.base_url, "-p", "hi"],
            &env,
        ),
        "",
    )
    .await;

    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "hello jsonl\n", "stdout carries only the model text");
    assert!(
        !err.contains("\"live\""),
        "no live line may leak to stderr either: {err}"
    );
}

/// The golden stdout: every durable line is byte-identical to the journal file, live lines
/// carry only their deltas, and the `Result` line matches its fixture's field set.
#[tokio::test]
async fn jsonl_stdout_is_durable_lines_plus_live_plus_result() {
    let server = MockServer::start(vec![text_reply_with_usage("hello jsonl", 10, 4)]).await;
    let env = Env::fresh("golden");
    let (code, durable, live, result, err) =
        run_jsonl(&server.base_url, &env, "golden", &[], "hi", "").await;

    assert_eq!(code, 0, "{err}");
    // Every durable line is byte-identical to the journal file: the file's bytes are the
    // durable lines joined with newlines.
    let journal = std::fs::read(env.sessions.join("golden.jsonl")).unwrap();
    let expected = durable.join("\n") + "\n";
    assert_eq!(
        expected.as_bytes(),
        journal.as_slice(),
        "durable stdout lines must equal the journal file byte for byte"
    );
    assert!(
        !durable.is_empty() && journal.starts_with(b"{\"schema_version\":1"),
        "the stream starts with the SessionCreated line from before the tee"
    );
    // Live lines carry only their delta text.
    assert!(!live.is_empty(), "the streamed text arrives as live lines");
    for line in &live {
        assert_eq!(line.get("live"), Some(&Value::Bool(true)));
        assert!(
            line.get("event") == Some(&Value::String("ModelTextDelta".to_owned())),
            "only text deltas streamed here: {line}"
        );
        assert!(line.get("data").and_then(|data| data.get("text")).is_some());
    }
    let streamed: String = live
        .iter()
        .filter_map(|line| {
            line.get("data")
                .and_then(|data| data.get("text"))
                .and_then(|text| text.as_str())
        })
        .collect();
    assert_eq!(streamed, "hello jsonl");
    // The `Result` line folds the run and the session totals.
    assert_eq!(
        result,
        json!({
            "type": "Result",
            "session_id": "golden",
            "outcome": "completed",
            "exit_code": 0,
            "stop_reason": "completed",
            "turns": 1,
            "usage": {"input": 10, "output": 4},
            "cost": Value::Null,
            "run_turns": 1,
            "run_usage": {"input": 10, "output": 4},
            "run_cost": Value::Null,
            "text": "hello jsonl",
        }),
        "the Result line carries the run and the session totals"
    );
    // ... and its field set is the checked-in fixture's field set (each fixture line).
    let fixture_text =
        std::fs::read_to_string(repo_root().join("docs/schema/fixtures/exec-result.jsonl"))
            .unwrap();
    for (i, line) in fixture_text.lines().enumerate() {
        let fixture: Value = serde_json::from_str(line).unwrap();
        let mut result_keys: Vec<&str> = result
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut fixture_keys: Vec<&str> = fixture
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        result_keys.sort_unstable();
        fixture_keys.sort_unstable();
        assert_eq!(
            result_keys, fixture_keys,
            "the Result line matches its fixture line {i}"
        );
    }
}

/// Runs one `-p` turn with `--output jsonl` on a resumed session and splits stdout the
/// same way as [`run_jsonl`].
async fn run_jsonl_resume(
    server_base_url: &str,
    env: &Env,
    session_id: &str,
    prompt: &str,
) -> (i32, Vec<String>, Vec<Value>, Value, String) {
    let args = vec![
        "--model",
        "m",
        "--base-url",
        server_base_url,
        "--resume",
        session_id,
        "--output",
        "jsonl",
        "-p",
        prompt,
    ];
    let (code, out, err) = feed(spawn(&args, env), "").await;
    let mut lines: Vec<String> = out.lines().map(str::to_owned).collect();
    assert!(
        !lines.is_empty(),
        "jsonl stdout must end with a Result line"
    );
    let result: Value = serde_json::from_str(lines.pop().unwrap().as_str())
        .expect("the last stdout line is the Result record");
    assert_eq!(
        result.get("type"),
        Some(&Value::String("Result".to_owned())),
        "the last line is the Result record: {result}"
    );
    let mut durable = Vec::new();
    let mut live = Vec::new();
    for line in &lines {
        let value: Value =
            serde_json::from_str(line).expect("every stdout line is one JSON object");
        if value.get("live") == Some(&Value::Bool(true)) {
            live.push(value);
        } else {
            durable.push(line.clone());
        }
    }
    (code, durable, live, result, err)
}

/// A resumed run reports session totals across both turns but `run_*` only for this
/// process. Mutations: `run_*` folded over the whole file, `run_*` copied from the
/// session totals, or the tee boundary off by one line → the run half equals the
/// session half (or misses a turn) and this fails.
#[tokio::test]
async fn resume_run_usage_counts_only_this_process() {
    let server = MockServer::start(vec![
        text_reply_with_usage("first", 10, 4),
        text_reply_with_usage("second", 5, 2),
    ])
    .await;
    let env = Env::fresh("resume-run");
    let (code, _, _, first_result, err) =
        run_jsonl(&server.base_url, &env, "rsess", &[], "first", "").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(first_result.get("turns"), Some(&json!(1)));
    assert_eq!(first_result.get("run_turns"), Some(&json!(1)));

    let (code, _, _, result, err) =
        run_jsonl_resume(&server.base_url, &env, "rsess", "second").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        result.get("turns"),
        Some(&json!(2)),
        "session covers both turns"
    );
    assert_eq!(
        result.get("run_turns"),
        Some(&json!(1)),
        "this process ran one turn: {result}"
    );
    assert_eq!(
        result.get("usage"),
        Some(&json!({"input": 15, "output": 6})),
        "session usage covers both turns: {result}"
    );
    assert_eq!(
        result.get("run_usage"),
        Some(&json!({"input": 5, "output": 2})),
        "run usage is only the second turn: {result}"
    );
    // No catalog price in tests: no fee record, so both costs are null.
    assert_eq!(result.get("cost"), Some(&Value::Null));
    assert_eq!(result.get("run_cost"), Some(&Value::Null));
    assert_eq!(result.get("text"), Some(&json!("second")));
}

/// Without `--resume` the two groups are byte-identical in the `Result` line.
#[tokio::test]
async fn no_resume_run_groups_equal_session_groups() {
    let server = MockServer::start(vec![text_reply_with_usage("hello", 10, 4)]).await;
    let env = Env::fresh("no-resume-equal");
    let (code, _, _, result, err) = run_jsonl(&server.base_url, &env, "equal", &[], "hi", "").await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        result.get("run_turns"),
        result.get("turns"),
        "run_turns equals turns without a resume: {result}"
    );
    assert_eq!(
        result.get("run_usage"),
        result.get("usage"),
        "run_usage equals usage without a resume: {result}"
    );
    assert_eq!(
        result.get("run_cost"),
        result.get("cost"),
        "run_cost equals cost without a resume: {result}"
    );
}

/// Exit 0 is a completed turn. Mutation: map `Completed` to 1 → this fails.
#[tokio::test]
async fn a_completed_turn_exits_zero() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let env = Env::fresh("exit-zero");
    let (code, _, _, result, err) = run_jsonl(&server.base_url, &env, "zero", &[], "hi", "").await;

    assert_eq!(code, 0, "{err}");
    assert_eq!(result.get("exit_code"), Some(&json!(0)));
    assert_eq!(result.get("outcome"), Some(&json!("completed")));
    assert_eq!(result.get("stop_reason"), Some(&json!("completed")));
}

/// Exit 1 is a failed turn. Mutation: map `Failed` back to 1's neighbors → this fails.
#[tokio::test]
async fn a_failed_turn_exits_one() {
    let server = MockServer::start(vec![Reply::status(400, "provider is down")]).await;
    let env = Env::fresh("exit-one");
    let (code, _, _, result, err) = run_jsonl(&server.base_url, &env, "one", &[], "hi", "").await;

    assert_eq!(code, 1, "{err}");
    assert_eq!(result.get("exit_code"), Some(&json!(1)));
    assert_eq!(result.get("outcome"), Some(&json!("failed")));
    assert_eq!(result.get("stop_reason"), Some(&json!("failed")));
}

/// Exit 3 is the budget family. Mutation: map `BudgetExceeded` back to 1 → this fails.
#[tokio::test]
async fn an_exhausted_budget_exits_three() {
    // One response that declares a tool call, with the limit at 1: the turn ends on the
    // budget before the call runs, so no tool needs to exist.
    let server = MockServer::start(vec![tool_reply("c1", "read", "{}")]).await;
    let env = Env::fresh("exit-three");
    let (code, _, _, result, err) = run_jsonl(
        &server.base_url,
        &env,
        "three",
        &["--max-model-calls", "1"],
        "hi",
        "",
    )
    .await;

    assert_eq!(code, 3, "{err}");
    assert_eq!(result.get("exit_code"), Some(&json!(3)));
    assert_eq!(result.get("outcome"), Some(&json!("budget_exceeded")));
    assert_eq!(result.get("stop_reason"), Some(&json!("budget")));
}

/// Exit 4 is an unfinished answer. Mutation: map either arm back to 1 → this fails.
#[tokio::test]
async fn an_unfinished_answer_exits_four() {
    // MaxTokens: the model stopped for length with no tool calls to run.
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"role": "assistant", "content": "partial"}), None),
        chunk(json!({}), Some("length")),
    ])]);
    let server = server.await;
    let env = Env::fresh("exit-four-maxtokens");
    let (code, _, _, result, err) =
        run_jsonl(&server.base_url, &env, "four-tokens", &[], "hi", "").await;

    assert_eq!(code, 4, "{err}");
    assert_eq!(result.get("exit_code"), Some(&json!(4)));
    assert_eq!(result.get("outcome"), Some(&json!("incomplete")));
    assert_eq!(result.get("stop_reason"), Some(&json!("max_tokens")));
}

/// Exit 4 is a refused answer. Mutation: map `Refused` back to 1 → this fails.
#[tokio::test]
async fn a_refused_answer_exits_four() {
    let server = MockServer::start(vec![Reply::sse(&[
        chunk(json!({"refusal": "I can't help with that."}), None),
        chunk(json!({"refusal": " I can't help with that."}), Some("stop")),
    ])]);
    let server = server.await;
    let env = Env::fresh("exit-four-refused");
    let (code, _, _, result, err) =
        run_jsonl(&server.base_url, &env, "four-refused", &[], "hi", "").await;

    assert_eq!(code, 4, "{err}");
    assert_eq!(result.get("exit_code"), Some(&json!(4)));
    assert_eq!(result.get("outcome"), Some(&json!("refused")));
    assert_eq!(result.get("stop_reason"), Some(&json!("refused")));
}

/// `-p -` reads the prompt from stdin to EOF: the whole pipe arrives as the one user
/// message. Mutation: start the turn before EOF (read one line) → the request carries
/// only the first line and this fails.
#[tokio::test]
async fn dash_p_reads_the_prompt_from_stdin_to_eof() {
    let server = MockServer::start(vec![text_reply("done")]).await;
    let env = Env::fresh("dash-p");
    let (code, out, err) = feed(
        spawn(
            &["--model", "m", "--base-url", &server.base_url, "-p", "-"],
            &env,
        ),
        "the stdin prompt\nsecond line",
    )
    .await;

    assert_eq!(code, 0, "{err}");
    assert!(out.contains("done"), "{out}");
    let body = server.requests()[0].json();
    let body_text = body.to_string();
    assert!(
        body_text.contains("the stdin prompt\\nsecond line"),
        "the whole pipe is the one prompt: {body_text}"
    );
}

/// `--help` names the `--output` format and the `-p -` stdin rule.
#[tokio::test]
async fn exec_help_names_the_output_format_and_dash_p() {
    let env = Env::fresh("help");
    let (code, out, err) = feed(spawn(&["--help"], &env), "").await;
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("--output"), "{out}");
    assert!(out.contains("jsonl"), "{out}");
    assert!(out.contains("stdin"), "{out}");
}

/// A minimal PNG header with these dimensions (24 bytes).
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0, 0, 0, 0]);
    bytes
}

/// The user `MessageAppended` line of a jsonl run.
fn user_message(durable: &[String]) -> Value {
    durable
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| {
            value["event"] == "MessageAppended" && value["data"]["message"].get("user").is_some()
        })
        .expect("a durable user message")
}

/// `--image` attaches each image to the `-p` prompt's user message, in order, and the provider
/// request carries it (Q7, ADR 0032 A2).
#[tokio::test]
async fn exec_image_attaches_to_the_prompt_user_message_in_order() {
    let server = MockServer::start(vec![text_reply("ok")]).await;
    let env = Env::fresh("image");
    let files = scratch("image-files");
    let first = files.join("a.png");
    let second = files.join("b.png");
    std::fs::write(&first, png(10, 10)).unwrap();
    std::fs::write(&second, png(20, 30)).unwrap();
    let (code, durable, _live, _result, err) = run_jsonl(
        &server.base_url,
        &env,
        "sid-image",
        &[
            "--image",
            first.to_str().unwrap(),
            "--image",
            second.to_str().unwrap(),
        ],
        "describe",
        "",
    )
    .await;
    assert_eq!(code, 0, "{err}");
    let user = user_message(&durable);
    let content = user["data"]["message"]["user"]["content"]
        .as_array()
        .expect("content parts");
    assert_eq!(content[0]["text"]["text"], "describe");
    assert_eq!(content[1]["image"]["mime_type"], "image/png");
    assert_eq!(content[1]["image"]["annotations"]["name"], "a.png");
    assert_eq!(content[2]["image"]["annotations"]["name"], "b.png");
    assert_eq!(content[2]["image"]["annotations"]["height"], 30);

    // The provider request has the images as data URLs (openai-compatible).
    let body = server.requests()[0].json().to_string();
    assert!(body.contains("data:image/png;base64,"), "{body}");
}

/// A `--image` that is not an image (or is over a limit) is a usage error: exit 2, naming the
/// path and the reason (ADR 0032 A3).
#[tokio::test]
async fn a_bad_image_is_a_usage_error() {
    let server = MockServer::start(Vec::new()).await;
    let env = Env::fresh("bad-image");
    let files = scratch("bad-image-files");
    let bad = files.join("notes.png");
    std::fs::write(&bad, "plain text").unwrap();
    let (code, _out, err) = feed(
        spawn(
            &[
                "--model",
                "m",
                "--base-url",
                &server.base_url,
                "--image",
                bad.to_str().unwrap(),
                "-p",
                "hi",
            ],
            &env,
        ),
        "",
    )
    .await;
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("--image"), "{err}");
    assert!(err.contains("notes.png"), "{err}");
    assert!(err.contains("not a PNG"), "{err}");
}
