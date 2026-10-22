//! The hidden `--deny-read` flag (ADR 0048 decision 5): extra paths join the sandbox's
//! deny-read list and the file tools' deny set. It is internal: `radian arc work` sets it on
//! a worker. The `full` tier reads any absolute path except the deny set, so these two tests
//! show the flag is what refuses the path, not the tier.

mod common;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_provider_openai_compatible::testing::{MockServer, Reply, chunk};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// A hang watchdog, not a performance assertion (the same shape as `jsonl.rs`'s).
const EXIT_WATCHDOG: Duration = Duration::from_secs(120);

fn scratch(prefix: &str) -> PathBuf {
    static DIRS: AtomicU64 = AtomicU64::new(0);
    // /tmp, not $TMPDIR: on macOS the latter can push <home>/run/<pid>.sock past SUN_LEN (104).
    let dir = PathBuf::from("/tmp").join(format!(
        "radian-exec-deny-read-{prefix}-{}-{}",
        std::process::id(),
        DIRS.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Env {
    home: PathBuf,
    sessions: PathBuf,
    ws: PathBuf,
    denied: PathBuf,
}

impl Env {
    fn fresh(prefix: &str) -> Self {
        let home = scratch(&format!("{prefix}-home"));
        let sessions = scratch(&format!("{prefix}-sessions"));
        let ws = scratch(&format!("{prefix}-ws"));
        let denied = scratch(&format!("{prefix}-denied"));
        Self {
            home,
            sessions,
            ws,
            denied,
        }
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

/// Runs one `full`-tier `read` of a file outside the workspace, with `--deny-read` when
/// `deny` is set, and returns the exit code and every request body the mock saw.
async fn read_outside(deny: bool) -> (i32, String) {
    let env = Env::fresh(if deny { "denied" } else { "allowed" });
    let secret = env.denied.join("secret.txt");
    std::fs::write(&secret, "TOPSECRET-DENY-READ").unwrap();
    let server = MockServer::start(vec![
        tool_reply(
            "c1",
            "read",
            &json!({"path": secret.to_str().unwrap()}).to_string(),
        ),
        text_reply("done"),
    ])
    .await;
    let mut args = vec![
        "--model",
        "m",
        "--base-url",
        &server.base_url,
        "--workspace",
        env.ws.to_str().unwrap(),
        "--yes",
        "--session-id",
        if deny { "denied" } else { "allowed" },
    ];
    if deny {
        args.push("--deny-read");
        args.push(env.denied.to_str().unwrap());
    }
    args.extend_from_slice(&["-p", "read it"]);
    let (code, _, err) = feed(spawn(&args, &env), "").await;
    let bodies = server
        .requests()
        .iter()
        .map(|request| request.json().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!bodies.is_empty(), "{err}");
    (code, bodies)
}

/// In the `full` tier the same absolute path reads; `--deny-read` refuses it and names the
/// deny set. Mutation: drop the flag from the sandbox list or from the file tools' deny set
/// → the secret content reaches the model and this fails.
#[tokio::test]
async fn deny_read_refuses_the_path_in_the_full_tier() {
    let (code, bodies) = read_outside(true).await;
    assert_eq!(code, 0);
    assert!(
        !bodies.contains("TOPSECRET-DENY-READ"),
        "the denied file's content reached the model"
    );
    assert!(bodies.contains("deny set"), "{bodies}");
}

/// Without the flag the `full` tier reads the same path: the refusal above is the flag's,
/// not the tier's.
#[tokio::test]
async fn without_deny_read_the_full_tier_reads_the_same_path() {
    let (code, bodies) = read_outside(false).await;
    assert_eq!(code, 0);
    assert!(bodies.contains("TOPSECRET-DENY-READ"), "{bodies}");
}
