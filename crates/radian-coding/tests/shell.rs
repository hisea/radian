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

use agent_core::{
    CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolOutput, ToolProvider,
    ToolSession,
};
use common::{TempDir, sentinel_outside};
use radian_coding::{CodingTools, CodingToolsError, Limits};
use radian_sandbox::{
    ProbeReport, Sandbox, SandboxBackend, SandboxError, SandboxMode, SandboxPolicy,
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
    ) -> Result<Command, SandboxError> {
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

    for mode in [SandboxMode::WorkspaceWrite, SandboxMode::Safe] {
        let error = Sandbox::from_backend(&FAILING, mode, &root).unwrap_err();
        assert!(
            matches!(error, SandboxError::ProbeFailed { .. }),
            "{mode:?}: {error}"
        );
    }
    // The only thing a failing backend can produce is a refusal; there is no value to hand
    // to `CodingTools::with_sandbox`, so no shell can exist.
    let tools = common::tools(&root);
    assert!(!spec_names(&tools).await.iter().any(|name| name == "shell"));
}

// ---------- 3. workspace-write confines the shell (each failure has a contrast) ----------

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

    // (d) connecting to a local port fails confined, succeeds unconfined.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local listener");
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "python3 -c 'import socket,sys; s=socket.create_connection((\"127.0.0.1\",{port}),5); s.close(); sys.exit(0)'"
    );
    let (ok, _) = sh(&confined, &script).await;
    assert!(!ok, "the sandbox has no network, so the connect must fail");
    let accept = std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let (ok, _) = sh(&open, &script).await;
    assert!(
        ok,
        "the control must connect to the listener this test owns"
    );
    let _ = accept.join();
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

// ---------- 9. full-access is never a default ----------

#[tokio::test]
async fn full_access_is_never_the_default() {
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
        spec.description.contains("NOT sandboxed"),
        "the description must not hide the lack of confinement: {}",
        spec.description
    );
    assert!(
        spec.description.contains("never a default"),
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
/// 3 times, filling the context with uncapped dumps. The description steers file viewing to
/// `read`/`list`, in the enforced mode and in the unsandboxed one.
#[tokio::test]
async fn the_shell_description_points_file_viewing_at_read_and_list() {
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
        assert!(
            spec.description
                .contains("prefer the `read` and `list` tools"),
            "{}",
            spec.description
        );
        checked += 1;
    }
    assert!(checked >= 1, "no shell was built to check");
}
