//! `StopFailure` and `Notification` in `radian exec` (ADR 0045 §6): a model error fires the
//! observe-only hook once with its kind, a matcher outside the closed set never runs, a
//! completed turn fires nothing, `exec -p` waits at most 5 s and then kills the run, and the
//! run's exit code and output are unchanged.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

const EXIT_WATCHDOG: Duration = Duration::from_secs(60);
const USAGE_LIMIT_BODY: &str =
    r#"{"error":{"type":"GoUsageLimitError","message":"monthly quota"}}"#;

fn scratch(prefix: &str) -> PathBuf {
    // The sandbox only writes inside the workspace, so `$TMPDIR` (which it points into the
    // repository) is the scratch root. Every config here turns the control port off, so no
    // long Unix socket path is involved.
    common::process_dir(&format!("radian-exec-hooks-{prefix}"))
}

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

async fn run(child: Child) -> (i32, String, String) {
    let mut child = child;
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

fn write_config(env: &Env, hooks: &str) -> PathBuf {
    let path = env.home.join("config.toml");
    std::fs::write(
        &path,
        format!("schema_version = 1\n[control]\nenabled = false\n[model]\nmodel = \"m\"\n{hooks}"),
    )
    .unwrap();
    path
}

async fn wait_for_file(path: &Path, what: &str) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never appeared at {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Edge case 1: a provider `429` classified `usage_limit` fires `StopFailure` with
/// `error: "usage_limit"`; a `usage_limit` matcher runs and a `transport` matcher does not;
/// the exit code is the same as the run without hooks.
#[tokio::test]
async fn a_usage_limit_turn_fires_only_the_matching_stop_failure() {
    let hit = scratch("usage-hit");
    std::fs::create_dir_all(&hit).unwrap();
    let env = Env::fresh("usage-limit");
    let config = write_config(
        &env,
        &format!(
            "[[hooks.StopFailure]]\nmatcher = \"usage_limit\"\n\
             [[hooks.StopFailure.hooks]]\ntype = \"command\"\ncommand = \"cat > '{}'\"\n\
             [[hooks.StopFailure]]\nmatcher = \"transport\"\n\
             [[hooks.StopFailure.hooks]]\ntype = \"command\"\ncommand = \"cat > '{}'\"\n",
            hit.join("hit.json").display(),
            hit.join("miss.json").display(),
        ),
    );
    let server = MockServer::start(vec![Reply::status(429, USAGE_LIMIT_BODY)]).await;
    let (code, out, err) = run(spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "usage-limit",
            "--config",
            config.to_str().unwrap(),
            "-p",
            "hi",
        ],
        &env,
    ))
    .await;
    assert!(out.is_empty(), "stdout carries only model text: {out}");
    assert_eq!(code, 1, "a failed turn exits 1: {err}");
    let text = wait_for_file(&hit.join("hit.json"), "the matched StopFailure hook").await;
    assert!(text.contains("\"error\":\"usage_limit\""), "{text}");
    assert!(
        text.contains("\"error_details\":\"model usage limit"),
        "{text}"
    );
    assert!(
        !hit.join("miss.json").exists(),
        "the transport matcher never runs"
    );

    // The same provider failure without hooks keeps the same exit code.
    let baseline = Env::fresh("usage-limit-baseline");
    let baseline_config = write_config(&baseline, "");
    let server = MockServer::start(vec![Reply::status(429, USAGE_LIMIT_BODY)]).await;
    let (baseline_code, baseline_out, baseline_err) = run(spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "usage-limit-baseline",
            "--config",
            baseline_config.to_str().unwrap(),
            "-p",
            "hi",
        ],
        &baseline,
    ))
    .await;
    assert_eq!(
        (code, out),
        (baseline_code, baseline_out),
        "hooks never change the exit code or output\nwith: {err}\nwithout: {baseline_err}"
    );
}

/// Edge case 2: a `StopFailure` hook that sleeps 30 s costs `exec -p` about 5 s, and the
/// hook's process group is killed before the process exits.
#[cfg(unix)]
#[tokio::test]
async fn p_waits_about_five_seconds_then_kills_a_slow_stop_failure_hook() {
    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only probes existence.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    let env = Env::fresh("slow-stop-failure");
    let pidfile = env.home.join("hook.pid");
    let config = write_config(
        &env,
        &format!(
            "[[hooks.StopFailure]]\n[[hooks.StopFailure.hooks]]\ntype = \"command\"\n\
             command = \"echo $$ > '{}'; sleep 30\"\n",
            pidfile.display()
        ),
    );
    let server = MockServer::start(vec![Reply::status(429, USAGE_LIMIT_BODY)]).await;
    let started = std::time::Instant::now();
    let mut child = spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "slow-stop-failure",
            "--config",
            config.to_str().unwrap(),
            "-p",
            "hi",
        ],
        &env,
    );
    child.stdin.take().unwrap().write_all(b"").await.unwrap();
    let pid: u32 = wait_for_file(&pidfile, "the StopFailure hook")
        .await
        .trim()
        .parse()
        .expect("the hook wrote its pid");
    let output = timeout(EXIT_WATCHDOG, child.wait_with_output())
        .await
        .expect("exec did not exit within the watchdog")
        .expect("wait");
    let elapsed = started.elapsed();
    assert_eq!(output.status.code(), Some(1), "{:?}", output);
    assert!(
        elapsed >= Duration::from_secs(4),
        "exec did not give the observer its 5 s: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "exec waited for the whole sleep: {elapsed:?}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the StopFailure hook's process group survived exec (pid {pid})"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Edge case 3: a `Completed` turn fires no `StopFailure`.
#[tokio::test]
async fn a_completed_turn_fires_no_stop_failure() {
    let env = Env::fresh("completed");
    let out = env.home.join("hook.json");
    let config = write_config(
        &env,
        &format!(
            "[[hooks.StopFailure]]\n[[hooks.StopFailure.hooks]]\ntype = \"command\"\n\
             command = \"cat > '{}'\"\n",
            out.display()
        ),
    );
    let server = MockServer::start(vec![text_reply("done")]).await;
    let (code, stdout, err) = run(spawn(
        &[
            "--model",
            "m",
            "--base-url",
            &server.base_url,
            "--session-id",
            "completed",
            "--config",
            config.to_str().unwrap(),
            "-p",
            "hi",
        ],
        &env,
    ))
    .await;
    assert_eq!(code, 0, "{err}");
    assert_eq!(stdout, "done\n", "{err}");
    assert!(!out.exists(), "a completed turn fires no StopFailure");
}
