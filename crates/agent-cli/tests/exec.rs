//! `radian exec` and the top-level management commands against the `agent` binary (ADR 0032
//! slice 1): the two entries are the same parser and the same dispatch, so their output, their
//! exit codes and their refusals must match. The binaries are built next to each other by the
//! workspace build.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion (the same shape as `cli.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

/// The `agent` binary, built by the harness.
fn agent_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agent")
}

/// The `radian` binary, next to `agent`. `cargo test -p agent-cli` alone does not necessarily
/// build it, so the failure says what to run.
fn radian_bin() -> PathBuf {
    let path = Path::new(agent_bin()).parent().unwrap().join("radian");
    assert!(
        path.exists(),
        "radian is not built; run: cargo build -p radian-tui --bin radian"
    );
    path
}

/// A fresh directory that no test shares, so nothing touches the user's real HOME, config or
/// credentials.
fn fresh_dir(prefix: &str) -> PathBuf {
    static DIRS: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agent-cli-exec-{prefix}-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One isolated environment: a HOME (also `RADIAN_HOME`) and a session dir. Two runs that are
/// compared share one, so a diagnostic that names the directory (the privacy warning) is
/// identical on both sides.
struct Env {
    home: PathBuf,
    sessions: PathBuf,
}

impl Env {
    fn fresh(prefix: &str) -> Self {
        let home = fresh_dir(prefix);
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        Self { home, sessions }
    }
}

/// Spawns `program` with `args` in `env`, plus a catalog URL pointing at a closed port, so no
/// run reads or writes the user's state or reaches the network.
fn spawn_in(program: &Path, args: &[&str], env: &Env) -> Child {
    Command::new(program)
        .args(args)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("RADIAN_CONFIG")
        .env("HOME", &env.home)
        .env("RADIAN_HOME", &env.home)
        .env("RADIAN_SESSION_DIR", &env.sessions)
        .env("RADIAN_CATALOG_URL", "http://127.0.0.1:1/api.json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the binary")
}

/// [`spawn_in`] in a fresh environment (for a test that compares nothing across runs).
fn spawn(program: &Path, args: &[&str]) -> Child {
    spawn_in(program, args, &Env::fresh("home"))
}

/// Writes no input, closes stdin, and waits with a cap so a stuck process fails the test.
async fn run(mut child: Child) -> (i32, String, String) {
    let _ = child.stdin.take().unwrap().write_all(b"").await;
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

#[tokio::test]
async fn radian_exec_one_shot_is_byte_identical_to_agent() {
    // One mock model serves both runs, so the only variable is the entry point.
    let server =
        MockServer::start(vec![text_reply("Hello there"), text_reply("Hello there")]).await;
    let args = ["--model", "m", "--base-url", &server.base_url, "-p", "hi"];

    let (agent_code, agent_out, _) = run(spawn(Path::new(agent_bin()), &args)).await;
    let radian = radian_bin();
    let (radian_code, radian_out, _) = run(spawn(
        &radian,
        &[
            "exec",
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "hi",
        ],
    ))
    .await;

    assert_eq!(agent_code, 0);
    assert_eq!(radian_code, agent_code, "the exit codes differ");
    assert_eq!(radian_out, agent_out, "the stdout differs");
    // The stdout contract: only the model's text, nothing else.
    assert_eq!(agent_out, "Hello there\n");
    assert_eq!(server.requests().len(), 2, "both runs reached the model");
}

#[tokio::test]
async fn radian_exec_list_matches_agent_list() {
    let env = Env::fresh("list");
    let (agent_code, agent_out, agent_err) =
        run(spawn_in(Path::new(agent_bin()), &["--list"], &env)).await;
    let (radian_code, radian_out, radian_err) =
        run(spawn_in(&radian_bin(), &["exec", "--list"], &env)).await;

    assert_eq!(agent_code, 0);
    assert_eq!(radian_code, agent_code);
    assert_eq!(radian_out, agent_out);
    assert_eq!(radian_err, agent_err);
}

#[tokio::test]
async fn radian_memory_matches_agent_memory() {
    // `memory list` needs no model, no key and no terminal: it is the cleanest command to
    // compare, and its output names the workspace both runs resolve from the same cwd.
    let env = Env::fresh("memory");
    let (agent_code, agent_out, agent_err) =
        run(spawn_in(Path::new(agent_bin()), &["memory", "list"], &env)).await;
    let (radian_code, radian_out, radian_err) =
        run(spawn_in(&radian_bin(), &["memory", "list"], &env)).await;

    assert_eq!(agent_code, radian_code, "the exit codes differ");
    assert_eq!(radian_out, agent_out, "the stdout differs");
    assert_eq!(radian_err, agent_err, "the stderr differs");
}

#[tokio::test]
async fn radian_exec_memory_is_the_same_as_the_top_level_memory() {
    // The direction: `radian memory …` and `radian exec memory …` are one code path, so
    // their output is identical - and identical to `agent memory …`.
    let env = Env::fresh("exec-memory");
    let (agent_code, agent_out, agent_err) =
        run(spawn_in(Path::new(agent_bin()), &["memory", "list"], &env)).await;
    let (exec_code, exec_out, exec_err) =
        run(spawn_in(&radian_bin(), &["exec", "memory", "list"], &env)).await;
    let (top_code, top_out, top_err) =
        run(spawn_in(&radian_bin(), &["memory", "list"], &env)).await;

    assert_eq!(top_code, exec_code);
    assert_eq!(exec_code, agent_code);
    assert_eq!(top_out, exec_out);
    assert_eq!(exec_out, agent_out);
    assert_eq!(top_err, exec_err);
    assert_eq!(exec_err, agent_err);
}

#[tokio::test]
async fn radian_login_refuses_without_a_terminal_like_agent() {
    // A fresh HOME means no credentials exist and none are written; without a terminal the
    // command refuses (exit 2) instead of reading a key from the pipe.
    let env = Env::fresh("login");
    let (agent_code, agent_out, agent_err) =
        run(spawn_in(Path::new(agent_bin()), &["login"], &env)).await;
    let (radian_code, radian_out, radian_err) =
        run(spawn_in(&radian_bin(), &["login"], &env)).await;

    assert_eq!(agent_code, 2);
    assert_eq!(radian_code, agent_code);
    assert_eq!(radian_out, agent_out);
    assert_eq!(radian_err, agent_err);
}

#[tokio::test]
async fn radian_serve_and_mcp_are_reserved_and_refuse_with_two() {
    for name in ["serve", "mcp"] {
        let (code, out, err) = run(spawn(&radian_bin(), &[name])).await;
        assert_eq!(code, 2, "{name}: a usage-class refusal");
        assert!(out.is_empty(), "{name}: stdout stays clean: {out}");
        assert!(
            err.contains("reserved for ADR 0030") && err.contains("not implemented"),
            "{name}: {err}"
        );
    }
}

#[tokio::test]
async fn a_tui_startup_refusal_is_a_usage_error() {
    // A config file that cannot be read refuses startup before the terminal is entered, so
    // this needs no tty. ADR 0032 A3: a startup refusal is 2 (it was 1 before).
    let dir = fresh_dir("bad-config");
    let config = dir.join("config.toml");
    std::fs::write(&config, "schema_version = 999\nnot = valid toml =\n").unwrap();
    let (code, out, err) = run(spawn(
        &radian_bin(),
        &["--config", config.to_str().unwrap()],
    ))
    .await;
    assert_eq!(code, 2, "a bad config is a usage/setup error: {err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("radian: "), "{err}");
}
