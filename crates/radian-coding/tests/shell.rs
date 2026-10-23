//! The `shell` tool: it runs only through a sandbox, cleans up its whole process group, caps
//! its output, and never hands the host's secrets to the command.
//!
//! Tests that need an enforcing backend call [`sandbox`], which returns `None` when the
//! machine has no working backend (for example a Linux box without bubblewrap). They then
//! **skip** instead of pretending to have checked confinement. The process-group, output-cap
//! and environment tests use a `FullAccess` sandbox, which needs no backend, so they run
//! everywhere.

mod common;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::{TempDir, sentinel_outside};
use radian_coding::{CodingTools, CodingToolsError, Limits};
use radian_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolOutput, ToolProvider,
    ToolSession,
};
use radian_sandbox::{
    ProbeReport, Sandbox, SandboxBackend, SandboxCommand, SandboxError, SandboxMode, SandboxPolicy,
    SandboxSlot,
};
use serde_json::{Value, json};

// ---------- helpers ----------

/// A sandbox for `mode`, or `None` when this machine has no working backend. `deny` adds
/// extra unreadable paths (the sensitive-read case uses a fake home, never a real one).
fn sandbox(mode: SandboxMode, ws: &Path, deny: &[PathBuf]) -> Option<Sandbox> {
    let policy = SandboxPolicy::new(mode, ws).ok()?;
    let policy = deny
        .iter()
        .fold(policy, |policy, path| policy.deny_read(path));
    match Sandbox::from_policy(policy) {
        Ok(sandbox) => Some(sandbox),
        Err(e) => {
            eprintln!("skipping: no working {mode:?} backend on this machine: {e}");
            None
        }
    }
}

fn tools_with(mode: SandboxMode, ws: &Path, deny: &[PathBuf]) -> Option<CodingTools> {
    let sandbox = sandbox(mode, ws, deny)?;
    Some(CodingTools::with_sandbox(ws, sandbox).expect("matching workspaces"))
}

/// `FullAccess` needs no backend, so it is the workhorse for tests about the tool's own
/// behaviour (process groups, caps, cwd, environment) rather than the sandbox's rules.
fn transparent_tools(ws: &Path) -> CodingTools {
    tools_with(SandboxMode::FullAccess, ws, &[]).expect("full-access needs no backend")
}

async fn call(tools: &CodingTools, name: &str, arguments: Value) -> Result<ToolOutput, String> {
    call_cancellable(tools, name, arguments, CancellationToken::new()).await
}

async fn call_cancellable(
    tools: &CodingTools,
    name: &str,
    arguments: Value,
    cancellation: CancellationToken,
) -> Result<ToolOutput, String> {
    let session = tools.open_session().await.expect("open session");
    let call = ToolCall {
        id: "t".into(),
        name: name.into(),
        arguments,
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation,
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .map_err(|failure| format!("{}: {}", failure.code, failure.message))
}

async fn spec_names(tools: &CodingTools) -> Vec<String> {
    tools
        .open_session()
        .await
        .expect("open session")
        .tool_specs()
        .iter()
        .map(|spec| spec.name.clone())
        .collect()
}

fn structured(output: &ToolOutput) -> Value {
    output.structured.clone().unwrap_or(json!(null))
}

fn output_text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks that the group has a member.
    unsafe { libc::killpg(pgid, 0) == 0 }
}

async fn gone_within(pid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn read_pid(dir: &Path, name: &str) -> i32 {
    std::fs::read_to_string(dir.join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{name} is not a pid: {e}"))
}

/// Waits until `predicate` holds, bounded. The sleep is the poll interval, never the
/// synchronization itself.
async fn wait_until(what: &str, predicate: impl Fn() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------- 1. no sandbox, no shell ----------

#[tokio::test]
async fn shell_is_absent_without_a_sandbox() {
    let dir = TempDir::new("shell-none");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = common::tools(&root);

    let names = spec_names(&tools).await;
    assert!(
        !names.iter().any(|name| name == "shell"),
        "a session with no sandbox must not advertise `shell`: {names:?}"
    );
    assert_eq!(tools.sandbox_mode(), None);

    // Even a hand-built call cannot run it: there is no fallback to an unsandboxed run.
    let failure = call(&tools, "shell", json!({"command": "echo hi"}))
        .await
        .unwrap_err();
    assert!(failure.starts_with("unknown_tool:"), "{failure}");
}

// ---------- 2. an unavailable backend never downgrades ----------

struct FailingBackend;

static FAILING: FailingBackend = FailingBackend;

impl SandboxBackend for FailingBackend {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Err(SandboxError::ProbeFailed {
            backend: "failing",
            detail: "the probe proved nothing is enforced".into(),
        })
    }

    fn command(
        &self,
        _policy: &SandboxPolicy,
        _program: &OsStr,
        _args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        Err(SandboxError::BackendUnavailable {
            backend: "failing",
            detail: "not available".into(),
        })
    }
}

#[tokio::test]
async fn an_unavailable_backend_never_yields_a_sandbox_or_a_shell() {
    let dir = TempDir::new("shell-failing");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();

    let mode = SandboxMode::WorkspaceWrite;
    let error = Sandbox::from_backend(&FAILING, mode, &root).unwrap_err();
    assert!(
        matches!(error, SandboxError::ProbeFailed { .. }),
        "{mode:?}: {error}"
    );
    // The only thing a failing backend can produce is a refusal; there is no value to hand
    // to `CodingTools::with_sandbox`, so no shell can exist.
    let tools = common::tools(&root);
    assert!(!spec_names(&tools).await.iter().any(|name| name == "shell"));
}

// ---------- 3. workspace confines the shell (each failure has a contrast) ----------

/// Runs one `sh -c` script through the tool and returns (succeeded, text).
async fn sh(tools: &CodingTools, script: &str) -> (bool, String) {
    let output = call(tools, "shell", json!({"command": script}))
        .await
        .unwrap_or_else(|e| panic!("shell failed to run: {e}"));
    let exit = structured(&output)["exit_code"].clone();
    (exit == json!(0), output_text(&output))
}

#[tokio::test]
async fn workspace_write_confines_the_shell() {
    let dir = TempDir::new("shell-confine");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    // A fake home: the sensitive-read case never touches a real credential.
    let fake_home = dir.file("fakehome");
    std::fs::create_dir_all(fake_home.join(".ssh")).unwrap();
    std::fs::write(
        fake_home.join(".ssh/id_ed25519"),
        "FAKE-KEY-NOT-A-REAL-SECRET\n",
    )
    .unwrap();

    let Some(confined) = tools_with(
        SandboxMode::WorkspaceWrite,
        &root,
        &[fake_home.join(".ssh")],
    ) else {
        return; // no working backend here: skip rather than pretend
    };
    let open = transparent_tools(&root); // the "sandbox off" contrast

    // (a) writing inside the workspace succeeds in both.
    let (ok, _) = sh(&confined, "echo inside > inside.txt").await;
    assert!(ok, "a workspace write must succeed under workspace-write");
    assert!(root.join("inside.txt").exists());

    // (b) writing outside fails confined, succeeds unconfined.
    let outside_target = outside.join("escape.txt");
    let (ok, _) = sh(
        &confined,
        &format!("echo escape > {}", outside_target.display()),
    )
    .await;
    assert!(!ok, "a write outside the workspace must fail");
    assert!(!outside_target.exists(), "the escape must not have landed");
    let (ok, _) = sh(
        &open,
        &format!("echo escape > {}", outside_target.display()),
    )
    .await;
    assert!(ok, "the same write must succeed with the sandbox off");

    // (c) reading a denied path fails confined, succeeds unconfined. The key is a fake one
    // in a scratch directory; the output goes to /dev/null and is never inspected.
    let key = fake_home.join(".ssh/id_ed25519");
    let (ok, _) = sh(&confined, &format!("cat {} > /dev/null", key.display())).await;
    assert!(!ok, "a denied read must fail");
    let (ok, _) = sh(&open, &format!("cat {} > /dev/null", key.display())).await;
    assert!(ok, "the same read must succeed with the sandbox off");

    // The default policy also denies the real `~/.ssh` (structural assertion: the mechanism
    // above is what enforces it, and no real key is read here).
    let policy = SandboxPolicy::workspace_write(&root).unwrap();
    assert!(
        policy.deny_read.iter().any(|path| path.ends_with(".ssh")),
        "the default deny list must cover ~/.ssh"
    );

    // (d) connecting to a local port succeeds in both (ADR 0016: the `workspace` tier opens
    // the network).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local listener");
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "python3 -c 'import socket,sys; s=socket.create_connection((\"127.0.0.1\",{port}),5); s.close(); sys.exit(0)'"
    );
    let (ok, _) = sh(&confined, &script).await;
    assert!(ok, "workspace has network on, so the connect must succeed");
    let (ok, _) = sh(&open, &script).await;
    assert!(
        ok,
        "the control must connect to the listener this test owns"
    );
}

// ---------- 4/5. timeout and cancellation kill the whole process group ----------

/// The script writes the leader and background pids, then sleeps. The background job is the
/// case that a "kill the direct child only" implementation would leave behind.
const TWO_SLEEPS: &str = "echo $$ > leader.pid; sleep 600 & echo $! > bg.pid; wait";

#[tokio::test]
async fn a_timeout_kills_the_whole_process_group() {
    let dir = TempDir::new("shell-timeout");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);

    let output = call(
        &tools,
        "shell",
        // Two seconds: the script writes both pids first, so a loaded machine must still
        // get them down before the timeout fires. The test would otherwise be timing-flaky
        // rather than testing the group kill.
        json!({"command": TWO_SLEEPS, "timeout_secs": 2}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    assert_eq!(structured["timed_out"], json!(true), "{structured}");
    assert!(output_text(&output).contains("timed out"), "{output:?}");
    // The message names the way out of the foreground cap (ADR 0037 decision 6).
    assert!(
        output_text(&output).contains("background: true"),
        "the timeout must point at background: true: {output:?}"
    );

    let leader = read_pid(&root, "leader.pid");
    let background = read_pid(&root, "bg.pid");
    assert_ne!(
        leader, background,
        "the background job must be a separate pid"
    );
    assert!(
        gone_within(background, Duration::from_secs(10)).await,
        "the background `sleep 600` (pid {background}) survived the timeout"
    );
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the shell (pid {leader}) survived the timeout"
    );
    assert!(
        !group_alive(leader),
        "the process group {leader} still has members"
    );
}

#[tokio::test]
async fn cancellation_kills_the_whole_process_group() {
    let dir = TempDir::new("shell-cancel");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let token = CancellationToken::new();

    let run = call_cancellable(
        &tools,
        "shell",
        json!({"command": TWO_SLEEPS, "timeout_secs": 600}),
        token.clone(),
    );
    let cancel = async {
        // Wait until both pid files hold a complete pid before cancelling. `exists()` alone
        // is not enough: the shell creates the file before writing the number, so cancelling
        // on existence would race a half-written file. The sleep inside the script keeps both
        // jobs alive until then.
        let pid_written = |name: &str| {
            std::fs::read_to_string(root.join(name))
                .ok()
                .and_then(|text| text.trim().parse::<i32>().ok())
                .is_some()
        };
        wait_until("the command to start both jobs", || {
            pid_written("leader.pid") && pid_written("bg.pid")
        })
        .await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(run, cancel);
    let output = result.unwrap();

    let structured = structured(&output);
    assert_eq!(structured["cancelled"], json!(true), "{structured}");
    assert!(output_text(&output).contains("cancelled"), "{output:?}");

    let leader = read_pid(&root, "leader.pid");
    let background = read_pid(&root, "bg.pid");
    assert!(
        gone_within(background, Duration::from_secs(10)).await,
        "the background `sleep 600` (pid {background}) survived the cancellation"
    );
    assert!(
        gone_within(leader, Duration::from_secs(10)).await,
        "the shell (pid {leader}) survived the cancellation"
    );
    assert!(
        !group_alive(leader),
        "the process group {leader} still has members"
    );
}

// ---------- 6. output cap ----------

/// A clamp note is about the request, not command stderr: it sits in the marker block before
/// `exit code:` (with `[timed out after …]` and `[cancelled …]`), names both numbers, and never
/// appears after `--- stderr ---`.
fn assert_clamp_before_exit(text: &str, marker: &str) {
    let marker_at = text
        .find(marker)
        .unwrap_or_else(|| panic!("missing {marker:?}: {text}"));
    let exit_at = text.find("exit code:").expect("an exit code line");
    assert!(
        marker_at < exit_at,
        "{marker:?} must precede `exit code:`:\n{text}"
    );
    let stderr_at = text.find("--- stderr ---").expect("a stderr section");
    assert!(
        !text[stderr_at..].contains(marker),
        "{marker:?} must not follow the stderr section:\n{text}"
    );
}

#[tokio::test]
async fn a_clamped_timeout_is_a_marker_before_exit_code_with_both_numbers() {
    let dir = TempDir::new("shell-timeout-clamp");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);

    let output = call(
        &tools,
        "shell",
        json!({"command": "echo CLAMP-CHECK", "timeout_secs": 900}),
    )
    .await
    .unwrap();
    let structured = structured(&output);
    assert_eq!(structured["timeout_clamped"], json!(true), "{structured}");
    assert_eq!(structured["timeout_secs"], json!(600), "{structured}");
    let text = output_text(&output);
    assert!(
        text.contains(
            "[timeout_secs 900 is above the maximum 600; the command ran with the 600 s limit]"
        ),
        "the note must name the request and the cap: {text}"
    );
    assert_clamp_before_exit(&text, "[timeout_secs 900 is above the maximum 600");

    // No clamp, no note.
    let plain = call(&tools, "shell", json!({"command": "echo CLAMP-CHECK"}))
        .await
        .unwrap();
    let plain = output_text(&plain);
    assert!(!plain.contains("is above the maximum"), "{plain}");
}

#[tokio::test]
async fn a_clamped_output_budget_is_a_marker_before_exit_code_with_both_numbers() {
    let dir = TempDir::new("shell-output-clamp");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let max = Limits::default().max_shell_output_bytes;
    let requested = max + 1;

    let output = call(
        &tools,
        "shell",
        json!({"command": "echo CLAMP-CHECK", "max_output_bytes": requested}),
    )
    .await
    .unwrap();
    let structured = structured(&output);
    assert_eq!(structured["output_clamped"], json!(true), "{structured}");
    assert_eq!(structured["output_bytes_per_stream"], json!(max));
    let text = output_text(&output);
    let marker = format!(
        "[max_output_bytes {requested} is above the maximum {max}; each stream was capped at \
         {max} bytes]"
    );
    assert!(text.contains(&marker), "{text}");
    assert_clamp_before_exit(&text, &marker);
    assert!(
        !text.contains("[timeout_secs "),
        "no timeout clamp without a timeout request: {text}"
    );

    let plain = call(&tools, "shell", json!({"command": "echo CLAMP-CHECK"}))
        .await
        .unwrap();
    let plain = output_text(&plain);
    assert!(!plain.contains("[max_output_bytes "), "{plain}");
}

/// `[timed out after …]` keeps its place in the same marker block, and no timeout-clamp note
/// appears below the cap.
#[tokio::test]
async fn a_timed_out_result_keeps_its_marker_before_exit_code() {
    let dir = TempDir::new("shell-timeout-marker");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);

    let output = call(
        &tools,
        "shell",
        json!({"command": "sleep 600", "timeout_secs": 1}),
    )
    .await
    .unwrap();
    let structured = structured(&output);
    assert_eq!(structured["timed_out"], json!(true), "{structured}");
    assert_eq!(structured["timeout_clamped"], json!(false), "{structured}");
    let text = output_text(&output);
    assert_clamp_before_exit(&text, "[timed out after");
    assert!(!text.contains("is above the maximum"), "{text}");
}

#[tokio::test]
async fn output_over_the_cap_is_truncated_and_reported() {
    let dir = TempDir::new("shell-cap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = sandbox(SandboxMode::FullAccess, &root, &[]).unwrap();
    let tools = CodingTools::with_sandbox(&root, sandbox)
        .unwrap()
        .with_limits(Limits {
            default_shell_output_bytes: 4096,
            max_shell_output_bytes: 4096,
            ..Limits::default()
        });

    let output = call(
        &tools,
        "shell",
        json!({"command": "i=0; while [ $i -lt 20000 ]; do echo line-$i; i=$((i+1)); done"}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    let truncated = structured["stdout_truncated_bytes"].as_u64().unwrap();
    assert!(
        truncated > 0,
        "the output must have been truncated: {structured}"
    );
    assert_eq!(structured["stdout_bytes"], json!(4096), "{structured}");
    assert_eq!(
        structured["stderr_truncated_bytes"],
        json!(0),
        "{structured}"
    );
    let text = output_text(&output);
    assert!(
        text.contains("bytes discarded") && text.contains(&truncated.to_string()),
        "the model must be told how much was discarded: {text}"
    );
    assert!(text.len() < 20_000, "the result must stay bounded");
}

#[tokio::test]
async fn the_end_of_a_long_output_is_kept() {
    // A failing build prints progress first and the error last. Keeping only the start would
    // drop exactly the part the model needs.
    let dir = TempDir::new("shell-tail");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 4096,
        max_shell_output_bytes: 4096,
        ..Limits::default()
    });

    let output = call(
        &tools,
        "shell",
        json!({
            "command": "i=0; while [ $i -lt 20000 ]; do echo filler-$i; i=$((i+1)); done; \
                        echo 'ERROR: the build failed at the very end'"
        }),
    )
    .await
    .unwrap();

    let text = output_text(&output);
    assert!(
        text.contains("ERROR: the build failed at the very end"),
        "the last line must survive truncation: {text}"
    );
    assert!(
        text.contains("bytes discarded"),
        "the middle must be marked as discarded: {text}"
    );
    assert!(
        text.contains("filler-0"),
        "the start is kept as well: {text}"
    );
}

#[tokio::test]
async fn each_stream_has_its_own_budget() {
    // stdout floods; the single stderr line that explains the failure must survive. With one
    // shared budget, stdout would consume it and the error would be dropped.
    let dir = TempDir::new("shell-streams");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 4096,
        max_shell_output_bytes: 4096,
        ..Limits::default()
    });

    let output = call(
        &tools,
        "shell",
        json!({
            "command": "i=0; while [ $i -lt 200000 ]; do echo flooding-$i; i=$((i+1)); done; \
                        echo 'ERROR: stderr survives' 1>&2"
        }),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    let text = output_text(&output);
    assert!(
        structured["stdout_truncated_bytes"].as_u64().unwrap() > 0,
        "stdout must be truncated: {structured}"
    );
    assert_eq!(
        structured["stderr_truncated_bytes"],
        json!(0),
        "stderr has its own budget and must not be truncated: {structured}"
    );
    assert!(
        text.contains("ERROR: stderr survives"),
        "the stderr line must survive: {text}"
    );
}

/// Peak resident set size in bytes, for the memory test below.
#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return None;
    }
    // `ru_maxrss` is bytes on macOS and kilobytes on Linux.
    #[cfg(target_os = "macos")]
    let scale = 1;
    #[cfg(not(target_os = "macos"))]
    let scale = 1024;
    Some(usage.ru_maxrss as u64 * scale)
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

#[tokio::test]
async fn memory_does_not_grow_with_the_output() {
    // 50 MiB through a 64 KiB-per-stream budget. The deterministic proof that the reader
    // retains only a head and a tail is the `read_capped` unit test; this checks the same
    // property end to end through a real process.
    let dir = TempDir::new("shell-mem");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);

    let before = peak_rss_bytes();
    let output = call(
        &tools,
        "shell",
        json!({
            "command": "yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
                        | head -c 52428800"
        }),
    )
    .await
    .unwrap();
    let after = peak_rss_bytes();

    let structured = structured(&output);
    assert_eq!(
        structured["stdout_total_bytes"],
        json!(52_428_800),
        "{structured}"
    );
    assert!(
        output_text(&output).len() < 200_000,
        "the returned text must stay bounded"
    );
    if let (Some(before), Some(after)) = (before, after) {
        let growth = after.saturating_sub(before);
        assert!(
            growth < 16 << 20,
            "peak RSS grew by {growth} bytes for a 50 MiB output"
        );
    }
}

// ---------- 7. environment ----------

#[tokio::test]
async fn the_host_environment_does_not_reach_the_shell() {
    let dir = TempDir::new("shell-env");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();

    // Pick a variable that really is set for this test process and is not on the sandbox
    // whitelist. Nothing is invented, and no real secret is needed.
    let probe = ["CARGO", "CARGO_MANIFEST_DIR", "RUST_BACKTRACE", "PWD"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some())
        .expect("a non-whitelisted variable is set for the test process");

    for tools in [transparent_tools(&root)] {
        let output = call(
            &tools,
            "shell",
            json!({"command": format!("printf '%s' \"${{{probe}-unset}}\"")}),
        )
        .await
        .unwrap();
        let text = output_text(&output);
        assert!(
            text.contains("unset"),
            "`{probe}` leaked into the shell: {text}"
        );
    }

    // The same under the enforcing mode, when this machine has a backend.
    if let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) {
        let output = call(
            &confined,
            "shell",
            json!({"command": format!("printf '%s' \"${{{probe}-unset}}\"")}),
        )
        .await
        .unwrap();
        assert!(
            output_text(&output).contains("unset"),
            "`{probe}` leaked into the confined shell"
        );
    }
}

// ---------- 8. cwd boundary ----------

#[tokio::test]
async fn cwd_escapes_are_refused() {
    let dir = TempDir::new("shell-cwd");
    let root = dir.file("ws");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let outside = sentinel_outside(&dir);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let tools = transparent_tools(&root);

    for cwd in ["/etc", "../outside", "sub/../../outside"] {
        let failure = call(&tools, "shell", json!({"command": "pwd", "cwd": cwd}))
            .await
            .unwrap_err();
        assert!(
            failure.starts_with("path_outside_workspace:"),
            "`{cwd}`: {failure}"
        );
    }
    #[cfg(unix)]
    {
        let failure = call(&tools, "shell", json!({"command": "pwd", "cwd": "link"}))
            .await
            .unwrap_err();
        assert!(
            failure.starts_with("path_outside_workspace:"),
            "a symlinked cwd: {failure}"
        );
    }

    // A workspace-relative directory works.
    let output = call(&tools, "shell", json!({"command": "pwd", "cwd": "sub"}))
        .await
        .unwrap();
    let printed = output_text(&output);
    assert!(
        printed.contains("ws/sub"),
        "the command must run in the subdirectory: {printed}"
    );
}

// ---------- 9. an unsandboxed shell requires an explicit sandbox ----------

#[tokio::test]
async fn an_unsandboxed_shell_requires_an_explicit_sandbox() {
    let dir = TempDir::new("shell-default");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();

    // The plain constructor has no sandbox and therefore no shell.
    let default = common::tools(&root);
    assert_eq!(default.sandbox_mode(), None);
    assert!(
        !spec_names(&default)
            .await
            .iter()
            .any(|name| name == "shell")
    );

    // Getting an unsandboxed shell requires naming `FullAccess` when building the sandbox.
    let open = transparent_tools(&root);
    assert_eq!(open.sandbox_mode(), Some(SandboxMode::FullAccess));
    let session = open.open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "shell")
        .expect("the full-access shell is advertised");
    assert!(
        spec.description.contains("runs with no sandbox"),
        "the description must not hide the lack of confinement: {}",
        spec.description
    );
    // ADR 0043: no sandbox is the built-in default now, and the description says so instead
    // of claiming the user chose it.
    assert!(
        spec.description.contains("built-in default"),
        "{}",
        spec.description
    );

    // A sandbox for a different directory is refused rather than silently used.
    let other = dir.file("other");
    std::fs::create_dir_all(&other).unwrap();
    let sandbox = sandbox(SandboxMode::FullAccess, &other, &[]).unwrap();
    let error = match CodingTools::with_sandbox(&root, sandbox) {
        Ok(_) => panic!("a sandbox for another directory must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, CodingToolsError::SandboxWorkspace { .. }),
        "{error}"
    );
}

/// Dogfooding D10: the model used `shell` (`cat -n`, `sed -n`) for 101 of 111 calls and `read`
/// 3 times, filling the context with uncapped dumps. A later session (2026-10-15) sent 38 reads
/// and searches through `shell` although `read`/`grep`/`glob` exist. The description steers
/// file reading and directory listing to `read` and searching to `grep`/`glob`, and says why
/// (`shell` runs alone; the read-only tools run in parallel), in the enforced mode and in the
/// unsandboxed one.
#[tokio::test]
async fn the_shell_description_points_file_reading_and_searching_at_the_read_only_tools() {
    let dir = TempDir::new("shell-spec-d10");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let mut checked = 0;
    for tools in [
        tools_with(SandboxMode::WorkspaceWrite, &root, &[]),
        Some(transparent_tools(&root)),
    ]
    .into_iter()
    .flatten()
    {
        let session = tools.open_session().await.unwrap();
        let spec = session
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "shell")
            .expect("shell is advertised with a sandbox");
        for wanted in [
            "Use `read` to read and list files",
            "Use `grep` and `glob` to search files",
            "They run in parallel",
            "`shell` runs alone",
        ] {
            assert!(
                spec.description.contains(wanted),
                "{wanted:?} is missing from {}",
                spec.description
            );
        }
        checked += 1;
    }
    assert!(checked >= 1, "no shell was built to check");
}

/// Items 6-9 (say-each-fact-once): the `shell` description kills the whole *process group*
/// (never "the whole session", which reads as the agent session) and states the one
/// cross-cutting rule the model otherwise cannot see (a foreground command never moves to
/// the background on its own: only an explicit `background: true` does); the per-parameter
/// numbers live in the schema fields only — the timeout default/maximum/count-from-start,
/// the 24 KiB per-stream cap, the background no-deadline rule — never in the description.
/// per-stream cap, the background no-deadline rule — never in the description. The `full` tier
/// names no ADR number and no `--profile` flag; and only `grep`/`glob` are said to run in
/// parallel (`read` has no blocking pool yet).
#[tokio::test]
async fn the_shell_description_states_the_timing_numbers_and_the_process_group() {
    let dir = TempDir::new("shell-spec-numbers");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let session = tools.open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "shell")
        .expect("shell is advertised with a sandbox");
    for wanted in [
        "kills the whole process group",
        "never moves to the background on its own",
        "only with an explicit `background: true`",
        "`grep` and `glob` report truncation",
        "They use less context",
        "They run in parallel",
    ] {
        assert!(
            spec.description.contains(wanted),
            "{wanted:?} is missing from {}",
            spec.description
        );
    }
    for gone in [
        "kills the whole session",
        "they report truncation",
        "(ADR 0043)",
        "--profile workspace",
        // The per-parameter numbers below live in the schema fields, not the description.
        "The default is 300 s",
        "The maximum is 600 s",
        "counts from the command's start",
        "default cap is 24 KiB",
        "ignores `timeout_secs`",
        "starts the command in the background",
        "has no deadline",
        "Do not sleep or poll",
        "end your reply",
        // No auto-background: none of the old move wording may remain.
        "The default threshold is",
        "The move keeps the same process",
        "returns at once with a job id",
        "You will be notified",
    ] {
        assert!(
            !spec.description.contains(gone),
            "{gone:?} must be gone from {}",
            spec.description
        );
    }
    // The schema fields carry the same numbers (the model reads them too). The kill rule
    // itself lives only in the tool description ("A timeout or a cancellation kills the
    // whole process group"), never in this field (ADR 0037: each fact once).
    let timeout = &spec.input_schema["properties"]["timeout_secs"]["description"];
    assert!(
        timeout
            .as_str()
            .is_some_and(|text| text.contains("Default: 300")
                && text.contains("Maximum: 600")
                && text.contains("counted from the command's start")
                && text.contains("ignores")
                && !text.contains("whole process group")),
        "the `timeout_secs` field must name the default, the maximum, the start and the \
         background rule, without repeating the kill rule: {timeout}"
    );
    let budget = &spec.input_schema["properties"]["max_output_bytes"]["description"];
    assert!(
        budget
            .as_str()
            .is_some_and(|text| text.contains("24 KiB") && text.contains("per stream")),
        "the `max_output_bytes` field must name the per-stream budget: {budget}"
    );
    let background = &spec.input_schema["properties"]["background"]["description"];
    assert!(
        background
            .as_str()
            .is_some_and(|text| text.contains("no deadline")
                && text.contains(
                    "Use for servers, watchers and commands expected to run longer than 600 s."
                )
                && text.contains("Run tests and builds in the foreground with timeout_secs.")),
        "the `background` field must say a background command has no deadline and when to use it: {background}"
    );
}

/// ADR 0020 decision 4: the sandbox's in-workspace temp directory is `.radian-sandbox/`, not
/// `.radian/`, so it cannot collide with `~/.radian/` when the workspace is `$HOME`.
#[tokio::test]
async fn the_sandbox_tmpdir_is_dot_radian_sandbox() {
    let dir = TempDir::new("shell-tmpdir");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return; // no working backend here: skip rather than pretend
    };
    let (ok, text) = sh(&confined, "printf '%s' \"$TMPDIR\"").await;
    assert!(ok, "{text}");
    assert!(text.contains(".radian-sandbox/tmp"), "{text}");
    assert!(
        root.join(".radian-sandbox/tmp").exists(),
        "the tmp directory must be inside the workspace"
    );
}

/// ADR 0033: a user's `!`/`!!` command runs through the **same** sandboxed executor as the
/// `shell` tool (`run_user_bash`), reporting stdout and the exit code the same way.
#[tokio::test]
async fn a_user_bash_runs_through_the_sandbox_and_reports_output() {
    let dir = TempDir::new("user-bash");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = sandbox(SandboxMode::FullAccess, &root, &[]).expect("full access");
    let out = radian_coding::run_user_bash(
        &root,
        &sandbox,
        "printf 'hello'; printf 'oops' >&2; exit 3",
        false,
        &CancellationToken::new(),
    )
    .await
    .expect("a full-access run");
    assert_eq!(out.stdout, "hello");
    assert_eq!(out.stderr, "oops");
    assert_eq!(out.exit_code, Some(3));
    assert!(!out.cancelled);
    assert!(!out.timed_out);
}

/// ADR 0033 + ADR 0012: the command really is confined when the tier enforces a sandbox — a
/// write outside the workspace fails. Skips when this machine has no working backend.
#[tokio::test]
async fn a_user_bash_is_confined_in_a_workspace_write_sandbox() {
    let dir = TempDir::new("user-bash-confined");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let Some(sandbox) = sandbox(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return; // no working backend here: skip rather than pretend
    };
    let target = outside.join("from-bang.txt");
    let out = radian_coding::run_user_bash(
        &root,
        &sandbox,
        &format!("echo x > {}", target.display()),
        false,
        &CancellationToken::new(),
    )
    .await
    .expect("the command ran (and failed)");
    assert_ne!(out.exit_code, Some(0), "the write must be denied");
    assert!(
        !target.exists(),
        "nothing may be written outside the workspace"
    );
}

/// ADR 0033: a `!` command honours the same cancellation the shell tool does: a pre-cancelled
/// token kills its process group and the outcome says so.
#[tokio::test]
async fn a_user_bash_honours_cancellation() {
    let dir = TempDir::new("user-bash-cancel");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = sandbox(SandboxMode::FullAccess, &root, &[]).expect("full access");
    let token = CancellationToken::new();
    token.cancel();
    let out = radian_coding::run_user_bash(&root, &sandbox, "sleep 30", false, &token)
        .await
        .expect("the run reported a cancellation");
    assert!(out.cancelled, "{out:?}");
}

// ---------- 9. the run-time tier switch (ADR 0038 slice 2) ----------

/// The sandbox is read from a shared slot at the start of every call: a swap takes effect on
/// the **next** call. A confined sandbox denies a write outside the workspace; after the slot
/// is swapped to `full`, the same command succeeds.
#[tokio::test]
async fn a_swapped_slot_changes_the_next_call() {
    let dir = TempDir::new("shell-slot");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let target = outside.join("escape.txt");

    let Some(confined) = sandbox(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return; // no working backend here: skip rather than pretend
    };
    let slot = SandboxSlot::with(confined);
    let tools = CodingTools::with_sandbox_slot(&root, slot.clone()).expect("matching workspaces");

    // Confined: the outside write is denied.
    let (ok, _) = sh(&tools, &format!("echo escape > {}", target.display())).await;
    assert!(
        !ok,
        "a write outside the workspace must fail while confined"
    );
    assert!(!target.exists(), "the escape must not have landed");

    // Swap to `full`; the very next call is unconfined.
    slot.set(Some(
        Sandbox::new(SandboxMode::FullAccess, &root).expect("full-access needs no backend"),
    ));
    let (ok, _) = sh(&tools, &format!("echo escape > {}", target.display())).await;
    assert!(ok, "after the swap the next call must be unconfined");
    assert!(
        target.exists(),
        "the write must land once the sandbox is off"
    );
}

// ---------- 10. the shell description tells the truth about `.git` (ADR 0038 slice 4) ----------

/// A backend whose **effective** git answer is fixed, so the description is testable under both
/// answers without a real probe and on any platform: the real Linux backend never opens `.git`,
/// and a bare `sandbox()` here would keep the read-only default. `git_writable_except_exec` is
/// what `CodingTools::open_session` reads (through the `Sandbox`), so this is the exact seam the
/// product uses.
struct GitBackend(bool);

impl SandboxBackend for GitBackend {
    fn name(&self) -> &'static str {
        "test-git"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Ok(ProbeReport {
            backend: self.name(),
            detail: "test".into(),
        })
    }

    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(&policy.workspace);
        Ok(SandboxCommand::new(command))
    }

    fn git_writable_except_exec(&self, _policy: &SandboxPolicy) -> bool {
        self.0
    }
}

static GIT_WRITABLE: GitBackend = GitBackend(true);
static GIT_READ_ONLY: GitBackend = GitBackend(false);

async fn shell_description(backend: &'static dyn SandboxBackend, root: &Path) -> String {
    let sandbox =
        Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, root).expect("a test sandbox");
    let tools = CodingTools::with_sandbox(root, sandbox).expect("matching workspaces");
    tools
        .open_session()
        .await
        .unwrap()
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "shell")
        .expect("shell is advertised with a sandbox")
        .description
        .clone()
}

/// ADR 0038 decision 4: with the writable carve-out in force the shell description says
/// native `git commit` works and the executable entries stay read-only; with the read-only
/// answer `.git` stays read-only and so does `git commit`. The two mutations this catches
/// are showing either text under the other answer.
#[tokio::test]
async fn the_shell_description_follows_the_effective_git_access() {
    let dir = TempDir::new("shell-git-text");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();

    let writable = shell_description(&GIT_WRITABLE, &root).await;
    assert!(writable.contains("`git commit` works"), "{writable}");
    assert!(writable.contains("stay read-only"), "{writable}");
    assert!(!writable.contains("(so is"), "{writable}");

    let read_only = shell_description(&GIT_READ_ONLY, &root).await;
    assert!(
        read_only.contains("repository's `.git` is read-only"),
        "{read_only}"
    );
    assert!(read_only.contains("`git commit`, by design"), "{read_only}");
    assert!(!read_only.contains("is writable"), "{read_only}");
}

// ---------- 7. default budget and failure tail (tool-small-fixes decision 2) ----------

/// 37-byte lines, so head/tail byte lengths land mid-line and the asserts below check exact
/// byte counts rather than line counts.
fn numbered_lines(count: usize, tail_command: &str) -> String {
    format!(
        "i=0; while [ $i -lt {count} ]; do printf 'out-%05d-abcdefghijklmnopqrstuvwxyz\\n' $i; \
         i=$((i+1)); done; {tail_command}"
    )
}

/// Splits the stdout section of a shell result into its (head, tail): the two sides of the
/// discarded-middle marker when truncated. Returns the whole section as the head when not
/// truncated.
fn stdout_view(text: &str) -> (String, String, bool) {
    let section = text
        .split("--- stdout ---\n")
        .nth(1)
        .expect("a stdout section")
        .split("--- stderr ---")
        .next()
        .expect("a stderr section");
    match section.split_once("\n[... ") {
        Some((head, rest)) => {
            let tail = rest
                .split_once("...]\n")
                .map(|(_, tail)| tail)
                .unwrap_or("");
            (head.to_owned(), tail.to_owned(), true)
        }
        None => (section.to_owned(), String::new(), false),
    }
}

#[tokio::test]
async fn default_output_budget_is_24kib_per_stream() {
    // No `with_limits`: the model's default budget. A command printing far more than the
    // budget is cut to exactly the new default, and the result names the discarded bytes.
    // On the old 64 KiB default `stdout_bytes` would be 65536, not 24576.
    let dir = TempDir::new("shell-default-cap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);

    let output = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(20_000, "exit 0")}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    assert_eq!(
        structured["output_bytes_per_stream"],
        json!(24 << 10),
        "{structured}"
    );
    assert_eq!(structured["stdout_bytes"], json!(24 << 10), "{structured}");
    let truncated = structured["stdout_truncated_bytes"].as_u64().unwrap();
    assert!(
        truncated > 100_000,
        "most of the output is cut: {structured}"
    );
    let text = output_text(&output);
    assert!(
        text.contains("bytes discarded") && text.contains(&truncated.to_string()),
        "the result names the discarded bytes: {text}"
    );
}

#[tokio::test]
async fn success_keeps_quarter_head_and_three_quarter_tail() {
    let dir = TempDir::new("shell-success-split");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 4096,
        max_shell_output_bytes: 4096,
        ..Limits::default()
    });

    let output = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(2000, "exit 0")}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    assert_eq!(structured["exit_code"], json!(0), "{structured}");
    assert_eq!(structured["stdout_bytes"], json!(4096), "{structured}");
    let text = output_text(&output);
    let (head, tail, truncated) = stdout_view(&text);
    assert!(
        truncated,
        "74_000 bytes through a 4096 budget must truncate"
    );
    assert_eq!(head.len(), 1024, "success keeps a 1/4 head");
    assert_eq!(tail.len(), 3072, "success keeps a 3/4 tail");
    assert!(head.starts_with("out-00000-"), "the start survives: {head}");
    assert!(
        tail.ends_with("out-01999-abcdefghijklmnopqrstuvwxyz\n"),
        "the end survives"
    );
}

#[tokio::test]
async fn failure_keeps_eighth_head_and_seven_eighth_tail() {
    // The same output as the success test, but a non-zero exit: the view keeps less start
    // (1/8) and more end (7/8) of the same budget. Rendered with the success split the head
    // would be 1024 bytes, not 512.
    let dir = TempDir::new("shell-failure-split");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 4096,
        max_shell_output_bytes: 4096,
        ..Limits::default()
    });

    let output = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(2000, "echo THIS-FAILED-AT-THE-END 1>&2; exit 1")}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    assert_eq!(structured["exit_code"], json!(1), "{structured}");
    assert_eq!(structured["stdout_bytes"], json!(4096), "{structured}");
    let text = output_text(&output);
    assert!(text.contains("exit code: 1"), "{text}");
    let (head, tail, truncated) = stdout_view(&text);
    assert!(
        truncated,
        "74_000 bytes through a 4096 budget must truncate"
    );
    assert_eq!(head.len(), 512, "failure keeps a 1/8 head");
    assert_eq!(tail.len(), 3584, "failure keeps a 7/8 tail");
    assert!(
        head.starts_with("out-00000-"),
        "some start survives: {head}"
    );
    assert!(
        tail.ends_with("out-01999-abcdefghijklmnopqrstuvwxyz\n"),
        "the very last line survives"
    );
    assert!(
        text.contains("THIS-FAILED-AT-THE-END"),
        "the stderr error line survives on its own budget: {text}"
    );
}

#[tokio::test]
async fn uneven_budgets_stay_within_cap_on_both_outcomes() {
    // A budget no split divides evenly: both views still show exactly the budget, never
    // more. If the failure render forgot to trim to its split, `stdout_bytes` would exceed
    // the cap here.
    let dir = TempDir::new("shell-uneven-cap");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 1000,
        max_shell_output_bytes: 1000,
        ..Limits::default()
    });

    let success = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(2000, "exit 0")}),
    )
    .await
    .unwrap();
    let (head, tail, _) = stdout_view(&output_text(&success));
    assert_eq!(head.len(), 250, "1/4 of 1000");
    assert_eq!(tail.len(), 750, "3/4 of 1000");
    assert_eq!(structured(&success)["stdout_bytes"], json!(1000));

    let failure = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(2000, "exit 1")}),
    )
    .await
    .unwrap();
    let (head, tail, _) = stdout_view(&output_text(&failure));
    assert_eq!(head.len(), 125, "1/8 of 1000");
    assert_eq!(tail.len(), 875, "7/8 of 1000");
    assert_eq!(structured(&failure)["stdout_bytes"], json!(1000));
}

#[tokio::test]
async fn failing_commands_last_line_is_always_present() {
    // The failure tail exists so the error at the end is never the part that is cut: even
    // at the smallest usable budget the last line is present.
    let dir = TempDir::new("shell-failure-last");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root).with_limits(Limits {
        default_shell_output_bytes: 256,
        max_shell_output_bytes: 256,
        ..Limits::default()
    });

    let output = call(
        &tools,
        "shell",
        json!({"command": numbered_lines(2000, "echo FINAL-MARKER-LAST-LINE; exit 3")}),
    )
    .await
    .unwrap();

    let structured = structured(&output);
    assert_eq!(structured["exit_code"], json!(3), "{structured}");
    let text = output_text(&output);
    assert!(
        text.contains("FINAL-MARKER-LAST-LINE"),
        "the last line survives at a 256-byte budget: {text}"
    );
    let (head, tail, _) = stdout_view(&text);
    assert_eq!(
        head.len() + tail.len(),
        256,
        "the view is still exactly the budget"
    );
}

// ---------- 11. slice 1: the sandbox-denial marker and `outside_sandbox` ----------

/// A write outside the workspace fails with the fixed marker (and the structured
/// category). Removing the marker from the report fails this test, so it is that
/// mutation's catcher. Skips without a working backend.
#[tokio::test]
async fn sandbox_denial_marks_an_outside_write() {
    let dir = TempDir::new("shell-denied-write");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return;
    };
    let target = outside.join("denied.txt");
    let output = call(
        &confined,
        "shell",
        json!({"command": format!("echo x > {}", target.display())}),
    )
    .await
    .expect("the tool ran (and the command failed)");
    let text = output_text(&output);
    assert!(
        text.contains("sandbox denied: filesystem-write"),
        "the marker is missing: {text}"
    );
    assert_eq!(
        structured(&output)["sandbox_denied"],
        json!("filesystem-write"),
        "{text}"
    );
    assert!(!target.exists(), "the escape must not have landed");
}

/// A write into a discovered `.git` entry is marked `git`, not plain
/// `filesystem-write`. Collapsing the two fails this test.
#[tokio::test]
async fn sandbox_denial_marks_a_git_write() {
    let dir = TempDir::new("shell-denied-git");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output();
    if !init.is_ok_and(|out| out.status.success()) {
        eprintln!("skipping: git init failed");
        return;
    }
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return;
    };
    let hook = root.join(".git").join("hooks").join("evil");
    let output = call(
        &confined,
        "shell",
        json!({"command": format!("echo x > {}", hook.display())}),
    )
    .await
    .expect("the tool ran (and the command failed)");
    let text = output_text(&output);
    assert!(
        text.contains("sandbox denied: git"),
        "the git marker is missing: {text}"
    );
    assert_eq!(
        structured(&output)["sandbox_denied"],
        json!("git"),
        "{text}"
    );
    assert!(!hook.exists(), "the hook must not have landed");
}

/// A read of a masked deny-read directory fails with ENOENT and is marked
/// `sensitive-read`. Dropping the deny-list arm fails this test.
#[tokio::test]
async fn sandbox_denial_marks_a_masked_sensitive_read() {
    let dir = TempDir::new("shell-denied-read");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let secret = dir.file("secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("key"), "s3cret").unwrap();
    let Some(confined) = tools_with(
        SandboxMode::WorkspaceWrite,
        &root,
        std::slice::from_ref(&secret),
    ) else {
        return;
    };
    let output = call(
        &confined,
        "shell",
        json!({"command": format!("cat {}", secret.join("key").display())}),
    )
    .await
    .expect("the tool ran (and the command failed)");
    let text = output_text(&output);
    assert!(
        text.contains("sandbox denied: sensitive-read"),
        "the sensitive-read marker is missing: {text}"
    );
    assert_eq!(
        structured(&output)["sandbox_denied"],
        json!("sensitive-read"),
        "{text}"
    );
}

/// A Unix-socket connect the sandbox denies is marked `unix-socket`. On Linux the
/// seccomp filter denies `socket(AF_UNIX, …)` itself; on macOS creating the socket
/// succeeds and only the connect is denied, so the probe connects to a listener held
/// outside the sandbox (a missing path would fail with ENOENT there instead, which is
/// honestly unmarked). Skips without a backend or without python3.
#[tokio::test]
async fn sandbox_denial_marks_a_unix_socket() {
    let dir = TempDir::new("shell-denied-socket");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return;
    };
    let probe = call(&confined, "shell", json!({"command": "command -v python3"}))
        .await
        .expect("the tool ran");
    if structured(&probe)["exit_code"] != json!(0) {
        eprintln!("skipping: no python3 in the sandbox");
        return;
    }
    // A listener outside the sandbox, under `/tmp` for the ~104-byte AF_UNIX path
    // limit; the sandboxed command connects to its canonical path.
    let sock_path =
        std::env::temp_dir().join(format!("radian-deny-sock-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock_path);
    #[cfg(unix)]
    let _listener = std::os::unix::net::UnixListener::bind(&sock_path).expect("bind a listener");
    let sock_path = std::fs::canonicalize(&sock_path).expect("canonical socket path");
    let output = call(
        &confined,
        "shell",
        json!({"command": format!(
            "python3 -c 'import socket; s=socket.socket(socket.AF_UNIX); s.connect(\"{}\")'",
            sock_path.display()
        )}),
    )
    .await
    .expect("the tool ran (and the command failed)");
    let text = output_text(&output);
    assert!(
        text.contains("sandbox denied: unix-socket"),
        "the unix-socket marker is missing: {text}"
    );
    assert_eq!(
        structured(&output)["sandbox_denied"],
        json!("unix-socket"),
        "{text}"
    );
}

/// An ordinary command failure carries no marker: marking everything would teach the
/// model its own bugs are the sandbox.
#[tokio::test]
async fn an_ordinary_failure_carries_no_marker() {
    let dir = TempDir::new("shell-no-marker");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return;
    };
    let output = call(&confined, "shell", json!({"command": "exit 3"}))
        .await
        .expect("the tool ran");
    let text = output_text(&output);
    assert!(!text.contains("sandbox denied"), "{text}");
    assert_eq!(structured(&output)["sandbox_denied"], json!(null), "{text}");
}

/// An approved `outside_sandbox` call runs unsandboxed for that one call: the outside
/// write lands, and the result names the reason. The permission layer is what forces
/// the `ask`; the tool's own job is only the single unsandboxed path plus the reason
/// check below.
#[tokio::test]
async fn outside_sandbox_runs_unsandboxed_for_one_call() {
    let dir = TempDir::new("shell-outside");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let outside = sentinel_outside(&dir);
    let Some(confined) = tools_with(SandboxMode::WorkspaceWrite, &root, &[]) else {
        return;
    };
    let target = outside.join("outside.txt");
    let output = call(
        &confined,
        "shell",
        json!({
            "command": format!("echo OUTSIDE > {}", target.display()),
            "outside_sandbox": true,
            "outside_sandbox_reason": "the test writes outside on purpose",
        }),
    )
    .await
    .expect("the outside call ran");
    let text = output_text(&output);
    assert!(
        structured(&output)["exit_code"] == json!(0),
        "the outside call must succeed: {text}"
    );
    assert!(
        text.contains("ran outside the sandbox")
            && text.contains("the test writes outside on purpose"),
        "the result must name the unsandboxed run and its reason: {text}"
    );
    assert!(
        !text.contains("sandbox denied"),
        "an unsandboxed run is never blamed on the sandbox: {text}"
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap_or_default().trim(),
        "OUTSIDE",
        "the outside write must have landed"
    );
    assert_eq!(structured(&output)["outside_sandbox"], json!(true));
}

/// `outside_sandbox: true` without a reason is refused before anything runs.
#[tokio::test]
async fn outside_sandbox_without_a_reason_is_refused() {
    let dir = TempDir::new("shell-outside-reason");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    for arguments in [
        json!({"command": "echo hi", "outside_sandbox": true}),
        json!({"command": "echo hi", "outside_sandbox": true, "outside_sandbox_reason": ""}),
    ] {
        let error = call(&tools, "shell", arguments)
            .await
            .expect_err("must be refused");
        assert!(
            error.contains("outside_sandbox_reason"),
            "the refusal must name the reason: {error}"
        );
    }
}

/// An unsandboxed background job is refused: the one unsandboxed path is foreground
/// only, so there is nowhere for such a job to run.
#[tokio::test]
async fn outside_sandbox_refuses_background() {
    let dir = TempDir::new("shell-outside-bg");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let tools = transparent_tools(&root);
    let error = call(
        &tools,
        "shell",
        json!({
            "command": "echo hi",
            "background": true,
            "outside_sandbox": true,
            "outside_sandbox_reason": "test",
        }),
    )
    .await
    .expect_err("must be refused");
    assert!(
        error.contains("foreground only"),
        "the refusal must say why: {error}"
    );
}

/// The `shell` description names `outside_sandbox` in one sentence, in both
/// `workspace` wordings. `full` has no sandbox to leave, so neither its description nor its
/// schema names the two fields — while `run` still parses with the full schema (see
/// `outside_sandbox_runs_unsandboxed_for_one_call`).
#[tokio::test]
async fn the_shell_description_names_outside_sandbox() {
    let dir = TempDir::new("shell-spec-outside");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let mut checked = 0;
    for tools in [
        tools_with(SandboxMode::WorkspaceWrite, &root, &[]),
        Some(transparent_tools(&root)),
    ]
    .into_iter()
    .flatten()
    {
        let session = tools.open_session().await.unwrap();
        let spec = session
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "shell")
            .expect("shell is advertised with a sandbox");
        let properties = &spec.input_schema["properties"];
        if spec.description.contains("runs with no sandbox") {
            assert!(
                !spec.description.contains("outside_sandbox"),
                "full has no sandbox to leave: {}",
                spec.description
            );
            assert!(
                properties.get("outside_sandbox").is_none()
                    && properties.get("outside_sandbox_reason").is_none(),
                "the full-tier schema must carry neither outside field"
            );
        } else {
            assert!(
                spec.description
                    .contains("set `outside_sandbox` and give `outside_sandbox_reason`"),
                "the workspace wording must name outside_sandbox: {}",
                spec.description
            );
            assert!(
                spec.description.contains("The call needs approval"),
                "the wording must say the call needs approval: {}",
                spec.description
            );
            assert!(
                properties.get("outside_sandbox").is_some()
                    && properties.get("outside_sandbox_reason").is_some(),
                "the workspace schema must carry both fields"
            );
        }
        checked += 1;
    }
    assert!(checked >= 1, "no shell was built to check");
}

/// The `full`-tier shell schema carries no `outside_sandbox`/`outside_sandbox_reason`
/// property, while **both** `workspace` variants (git-writable or not) carry them.
#[tokio::test]
async fn the_full_tier_shell_schema_has_no_outside_fields() {
    let dir = TempDir::new("shell-schema-tiers");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    let full = transparent_tools(&root);
    let session = full.open_session().await.unwrap();
    let spec = session
        .tool_specs()
        .iter()
        .find(|spec| spec.name == "shell")
        .expect("shell is advertised with a sandbox");
    let properties = &spec.input_schema["properties"];
    assert!(
        properties.get("outside_sandbox").is_none(),
        "the full-tier schema must not advertise outside_sandbox"
    );
    assert!(
        properties.get("outside_sandbox_reason").is_none(),
        "the full-tier schema must not advertise outside_sandbox_reason"
    );
    for backend in [&GIT_WRITABLE as &dyn SandboxBackend, &GIT_READ_ONLY] {
        let sandbox =
            Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, &root).expect("sandbox");
        let tools = CodingTools::with_sandbox(&root, sandbox).expect("matching workspaces");
        let spec = tools
            .open_session()
            .await
            .unwrap()
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "shell")
            .expect("shell is advertised with a sandbox")
            .clone();
        let properties = &spec.input_schema["properties"];
        assert!(
            properties.get("outside_sandbox").is_some()
                && properties.get("outside_sandbox_reason").is_some(),
            "the {} workspace schema must carry both outside fields",
            backend.name(),
        );
    }
}

/// Size budget (re-measured 2026-10-20, the fix1 STE rewrite): the serialized `workspace`
/// definition is about 2.73 KB with the writable carve-out wording, so the pin covers it
/// with a little room, the way the `read` (1100) and `ask_user` (1700) pins do.
#[tokio::test]
async fn the_shell_definition_stays_small() {
    let dir = TempDir::new("shell-size");
    let root = dir.file("ws");
    std::fs::create_dir_all(&root).unwrap();
    for backend in [&GIT_WRITABLE as &dyn SandboxBackend, &GIT_READ_ONLY] {
        let sandbox =
            Sandbox::from_backend(backend, SandboxMode::WorkspaceWrite, &root).expect("sandbox");
        let tools = CodingTools::with_sandbox(&root, sandbox).expect("matching workspaces");
        let spec = tools
            .open_session()
            .await
            .unwrap()
            .tool_specs()
            .iter()
            .find(|spec| spec.name == "shell")
            .expect("shell is advertised with a sandbox")
            .clone();
        let bytes = serde_json::to_vec(&spec).unwrap().len();
        assert!(
            bytes <= 2760,
            "the {} shell definition grew to {bytes} bytes, past the 2760 budget",
            backend.name(),
        );
    }
}
