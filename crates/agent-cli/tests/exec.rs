//! `radian exec` and the top-level management commands (ADR 0032): `radian memory`,
//! `radian login` and `radian exec memory`/`login` must be one code path, the reserved
//! subcommands must refuse, and the TUI's startup refusals must carry the unified exit code
//! and the `radian:` diagnostics.
//!
//! The `agent` binary is gone (ADR 0032 direction), so these tests drive `radian`
//! alone; `cli.rs` carries the migrated headless-command suite.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion (the same shape as `cli.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

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

/// Spawns `radian <args>` in `env`, plus a catalog URL pointing at a closed port, so no run
/// reads or writes the user's state or reaches the network.
fn spawn_in(args: &[&str], env: &Env) -> Child {
    Command::new(common::radian_bin())
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
        .expect("spawn radian")
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

#[tokio::test]
async fn the_top_level_memory_is_the_same_as_radian_exec_memory() {
    // The direction: `radian memory …` and `radian exec memory …` are one code path, so
    // their output is identical.
    let env = Env::fresh("exec-memory");
    let (exec_code, exec_out, exec_err) = run(spawn_in(&["exec", "memory", "list"], &env)).await;
    let (top_code, top_out, top_err) = run(spawn_in(&["memory", "list"], &env)).await;

    assert_eq!(top_code, exec_code);
    assert_eq!(top_out, exec_out);
    assert_eq!(top_err, exec_err);
}

#[tokio::test]
async fn the_top_level_login_is_the_same_as_radian_exec_login() {
    // A fresh HOME means no credentials exist and none are written; without a terminal the
    // command refuses (exit 2) instead of reading a key from the pipe.
    let env = Env::fresh("login");
    let (exec_code, exec_out, exec_err) = run(spawn_in(&["exec", "login"], &env)).await;
    let (top_code, top_out, top_err) = run(spawn_in(&["login"], &env)).await;

    assert_eq!(exec_code, 2);
    assert_eq!(top_code, exec_code);
    assert_eq!(top_out, exec_out);
    assert_eq!(top_err, exec_err);
}

#[tokio::test]
async fn radian_serve_and_mcp_are_reserved_and_refuse_with_two() {
    for name in ["serve", "mcp"] {
        let (code, out, err) = run(spawn_in(&[name], &Env::fresh("reserved"))).await;
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
    let env = Env::fresh("bad-config");
    let config = env.home.join("config.toml");
    std::fs::write(&config, "schema_version = 999\nnot = valid toml =\n").unwrap();
    let (code, out, err) = run(spawn_in(&["--config", config.to_str().unwrap()], &env)).await;
    assert_eq!(code, 2, "a bad config is a usage/setup error: {err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("radian: "), "{err}");
}

#[tokio::test]
async fn the_missing_credential_hint_names_radian_login() {
    // ADR 0032 direction: the hint is a user command, so it says `radian login …` (it
    // said `agent login …`). A non-local endpoint with no stored credential refuses startup
    // before any network call - no tty needed.
    let env = Env::fresh("credential-hint");
    let (code, out, err) = run(spawn_in(
        &[
            "--model",
            "m",
            "--provider",
            "openai-compatible",
            "--base-url",
            "https://example.invalid",
        ],
        &env,
    ))
    .await;

    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("radian login openai-compatible"),
        "the hint names the radian command: {err}"
    );
    assert!(
        !err.contains("agent login"),
        "no `agent login` hint remains: {err}"
    );
}

#[tokio::test]
async fn the_model_provider_hint_fires_once_without_a_provider_flag() {
    // `-m/--model` takes a bare model name; the form is `--provider <p>
    // --model <m>`. A known provider prefix with no `--provider` gets one hint — on the TUI's
    // startup log and on `radian exec`'s stderr — and the run still refuses for the missing
    // credential, which is where these stop (no network, no session file).
    let hint = "did you mean --provider opencode-go --model x?";
    let env = Env::fresh("model-hint");

    // The TUI's `-m`.
    let (code, out, err) = run(spawn_in(&["-m", "opencode-go/x"], &env)).await;
    assert_eq!(code, 2, "the credential refusal is a usage error: {err}");
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err.matches("did you mean").count(),
        1,
        "exactly one hint: {err}"
    );
    assert!(err.contains(hint), "{err}");

    // `radian exec --model`: the same hint on the same stderr path.
    let (code, out, err) = run(spawn_in(&["exec", "--model", "opencode-go/x"], &env)).await;
    assert_eq!(code, 2, "{err}");
    assert!(out.is_empty(), "{out}");
    assert_eq!(err.matches("did you mean").count(), 1, "{err}");

    // An explicit `--provider` is the supported spelling of a real `provider/model` id: no hint.
    let (_, _, err) = run(spawn_in(
        &[
            "exec",
            "--provider",
            "opencode-go",
            "--model",
            "anthropic/claude-x",
        ],
        &env,
    ))
    .await;
    assert!(!err.contains("did you mean"), "{err}");

    // A bare model name gets none either.
    let (_, _, err) = run(spawn_in(&["exec", "--model", "plain-name"], &env)).await;
    assert!(!err.contains("did you mean"), "{err}");
}

#[tokio::test]
async fn a_missing_workspace_is_refused_with_the_radian_prefix() {
    // A startup refusal names the program `radian:` (ADR 0032's prefix change) and exits 2.
    let env = Env::fresh("missing-workspace");
    let missing = Path::new("/definitely/not/a/workspace");
    let (code, _, err) = run(spawn_in(&["--workspace", missing.to_str().unwrap()], &env)).await;
    assert_eq!(code, 2, "{err}");
    assert!(
        err.starts_with("radian: ") || err.contains("\nradian: "),
        "{err}"
    );
    assert!(!err.contains("agent: "), "{err}");
}

/// ADR 0030 O6 (slice 8): a hosting process binds `$RADIAN_HOME/run/<pid>.sock`, names it on
/// the startup line, and removes it when the process exits.
#[tokio::test]
async fn the_control_socket_is_named_and_removed_when_the_process_exits() {
    use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};

    let server = MockServer::start(vec![Reply::sse(&[
        chunk(
            serde_json::json!({"role": "assistant", "content": "ok"}),
            None,
        ),
        chunk(serde_json::json!({}), Some("stop")),
    ])])
    .await;
    let env = Env::fresh("control-socket");
    let child = spawn_in(
        &[
            "exec",
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "-p",
            "hi",
        ],
        &env,
    );
    let pid = child.id().expect("the child pid");
    let socket = env.home.join("run").join(format!("{pid}.sock"));

    let (code, stdout, stderr) = run(child).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("ok"), "{stdout}");
    assert!(
        stderr.contains(&socket.display().to_string()),
        "the startup line names the socket at {socket:?}: {stderr}"
    );
    assert!(
        !socket.exists(),
        "the control socket is removed when the process exits: {socket:?}"
    );
}
