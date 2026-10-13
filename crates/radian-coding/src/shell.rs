//! The `shell` tool: a command run **through the sandbox**, never without one.
//!
//! The tool exists only when the session was built with a [`radian_sandbox::Sandbox`]
//! ([`crate::CodingTools::with_sandbox`]); a provider without one does not even advertise
//! it, so a model cannot ask for a naked run. `radian_sandbox` itself refuses to build an
//! enforced sandbox unless its probe proved the backend works (ADR 0012), so there is no
//! path from "backend unavailable" to "ran anyway".
//!
//! What is enforced, and what is not (stated, not papered over):
//!
//! - `Safe` / `WorkspaceWrite` confine the child (filesystem, network, sensitive reads) to
//!   what [`radian_sandbox`] proves. `FullAccess` is the caller's explicit "no confinement"
//!   choice: the command really is unsandboxed. The tool description says so in those words.
//! - Environment filtering happens on the spawn path. For enforced modes the backend clears
//!   the environment to the policy whitelist; for `FullAccess` the backend is transparent, so
//!   this module applies the same whitelist itself. A model-controlled shell never inherits
//!   the host's API keys in either mode.
//! - The command runs in its own process group, and the group is killed as soon as the
//!   leader exits (and on timeout or cancellation), so `sh -c 'sleep 600 &'` cannot leave a
//!   stray background process behind. A command that deliberately escapes the group with
//!   `setsid` is out of scope; the drain is bounded so even that cannot hang the tool.
//! - Output is capped; the model is told how many bytes were discarded.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agent_core::{ContentPart, ToolConcurrency, ToolContext, ToolFailure, ToolOutput, ToolSpec};
use radian_sandbox::{Sandbox, SandboxMode};
use serde::Deserialize;
use serde_json::json;

use crate::fail;
use crate::provider::Limits;
use crate::workspace::WorkspaceRoot;

/// How long the supervisor waits for the output readers after the group is killed. The
/// readers end at EOF once every writer is dead; the bound only matters if something escaped
/// the process group, and it keeps that from hanging the tool.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long the tool waits for the supervisor to reap the killed leader. SIGKILL is
/// immediate; the bound only covers thread scheduling.
const REAP_GRACE: Duration = Duration::from_secs(10);

/// The `shell` spec for `mode`. The description names the enforcement level, because the
/// model (and a human reading a transcript) must be able to tell a confined run from an
/// unsandboxed one.
pub(crate) fn spec(mode: SandboxMode) -> ToolSpec {
    let confinement = match mode {
        SandboxMode::WorkspaceWrite => {
            "The command runs under the OS sandbox in `workspace-write` mode: writes are \
             confined to the workspace, the repository's `.git` is read-only, sensitive \
             directories such as `~/.ssh` are not readable, and the network is off. \
             `git commit` fails by design (the sandbox keeps `.git` read-only)."
        }
        SandboxMode::Safe => {
            "The command runs under the OS sandbox in `safe` mode: the workspace is \
             read-only and the network is off."
        }
        SandboxMode::FullAccess => {
            "**The command is NOT sandboxed** (`full-access`): it can read and write \
             anywhere the user can, including the network. The user chose this mode \
             explicitly; it is never a default."
        }
    };
    ToolSpec {
        name: "shell".into(),
        description: format!(
            "Run a shell command with `/bin/sh -c`. {confinement} Each stream's output is \
             capped: past the budget the start and the end are kept and the middle is \
             replaced by a marker naming the discarded bytes (the end is always kept, \
             because that is where a failing command's error is). The process group is \
             killed on timeout or cancellation. Use `cwd` (workspace-relative) to run \
             somewhere other than the workspace root."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command; it is passed to `/bin/sh -c`."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How long the command may run before its process group is \
                                    killed. Clamped to the configured maximum."
                },
                "cwd": {
                    "type": "string",
                    "description": "Workspace-relative directory to run in (default: the \
                                    workspace root). Absolute paths, `..` and symlinks out \
                                    of the workspace are refused."
                },
                "max_output_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Output budget per stream (stdout and stderr each). Over \
                                    that, the start and the end are kept and the middle is \
                                    replaced by a marker; the end is always kept, because \
                                    that is where a failing command's error is. Clamped to \
                                    the configured maximum."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        // A shell command has side effects and is not safe to overlap with other calls.
        concurrency: ToolConcurrency::Exclusive,
    }
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
    /// Per-stream output budget; clamped to the hard cap.
    max_output_bytes: Option<usize>,
}

pub(crate) async fn run(
    root: &WorkspaceRoot,
    sandbox: &Sandbox,
    limits: &Limits,
    arguments: &serde_json::Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let args: ShellArgs = serde_json::from_value(arguments.clone())
        .map_err(|e| fail("invalid_arguments", e.to_string()))?;
    if args.command.trim().is_empty() {
        return Err(fail("invalid_arguments", "`command` must not be empty"));
    }
    // The cwd check is the same workspace boundary the file tools use: absolute paths, `..`
    // and symlinks out of the workspace are refused before anything is spawned.
    let cwd = match args.cwd.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => root.resolve_existing(dir)?,
        None => root.root().to_path_buf(),
    };
    if !cwd.is_dir() {
        return Err(fail(
            "not_a_directory",
            format!("`{}` is not a directory", args.cwd.unwrap_or_default()),
        ));
    }
    let (timeout, clamped) = clamp_timeout(args.timeout_secs, limits);
    let (output_cap, output_clamped) = clamp_output(args.max_output_bytes, limits);

    let outcome = execute(sandbox, &cwd, &args.command, timeout, output_cap, cx).await?;
    Ok(report(
        &args.command,
        root.root(),
        &cwd,
        sandbox.mode(),
        timeout,
        clamped,
        output_cap,
        output_clamped,
        &outcome,
    ))
}

fn clamp_timeout(requested: Option<u64>, limits: &Limits) -> (Duration, bool) {
    let max = limits.max_shell_timeout.as_secs().max(1);
    match requested {
        Some(secs) => {
            let clamped = secs > max;
            (Duration::from_secs(secs.clamp(1, max)), clamped)
        }
        None => (limits.default_shell_timeout, false),
    }
}

/// The per-stream output budget, mirroring `read`'s `max_bytes`: the model may ask for more,
/// inside the hard cap; the default matches `read`'s default budget.
fn clamp_output(requested: Option<usize>, limits: &Limits) -> (usize, bool) {
    let max = limits.max_shell_output_bytes.max(1);
    match requested {
        Some(bytes) => (bytes.clamp(1, max), bytes > max),
        None => (limits.default_shell_output_bytes.clamp(1, max), false),
    }
}

async fn execute(
    sandbox: &Sandbox,
    cwd: &Path,
    command: &str,
    timeout: Duration,
    output_cap: usize,
    cx: &ToolContext<'_>,
) -> Result<Outcome, ToolFailure> {
    let mut process = sandbox
        .command_str("/bin/sh", &["-c", command])
        .map_err(|e| {
            fail(
                "sandbox_unavailable",
                format!("could not build the sandboxed command: {e}"),
            )
        })?;
    process.current_dir(cwd);
    if !sandbox.mode().is_enforced() {
        // The transparent backend does not filter the environment. A model-controlled shell
        // still must not inherit the host's API keys, so the whitelist is applied here. For
        // enforced modes the backend already did it (and pointed TMPDIR into the workspace),
        // so re-applying it here would undo that.
        process.env_clear();
        for name in &sandbox.policy().allow_env {
            if let Some(value) = std::env::var_os(name) {
                process.env(name, value);
            }
        }
    }

    let started = Instant::now();
    let mut supervised = Supervised::spawn(&mut process, output_cap).map_err(|e| {
        fail(
            "shell_spawn_failed",
            format!("could not start the command: {e}"),
        )
    })?;

    let mut timed_out = false;
    let mut cancelled = false;
    let kill = tokio::select! {
        finished = supervised.finished() => !finished,
        () = tokio::time::sleep(timeout) => {
            timed_out = true;
            true
        }
        () = cx.cancellation.cancelled() => {
            cancelled = true;
            true
        }
    };
    if kill {
        supervised.kill_tree();
    }
    supervised.wait_done(REAP_GRACE).await;

    let mut outcome = supervised.take_outcome();
    outcome.timed_out = timed_out;
    outcome.cancelled = cancelled;
    outcome.duration = started.elapsed();
    Ok(outcome)
}

// ---------- supervision ----------

#[derive(Default)]
struct Capped {
    /// The first part of the stream, kept verbatim.
    head: Vec<u8>,
    /// The last part of the stream. A ring in spirit: it is trimmed as it grows, so memory
    /// never depends on how much the command printed.
    tail: Vec<u8>,
    /// Every byte this stream produced, kept or not.
    total: u64,
}

impl Capped {
    /// Bytes that are in neither `head` nor `tail`.
    fn discarded(&self) -> u64 {
        self.total - (self.head.len() + self.tail.len()) as u64
    }

    fn is_truncated(&self) -> bool {
        self.discarded() > 0
    }

    /// What the model sees: the head, a marker naming the discarded bytes, then the tail.
    /// The tail is what a failing build's error message is almost always in, so it is kept
    /// deliberately larger than the head.
    fn render(&self) -> String {
        if !self.is_truncated() {
            let mut bytes = self.head.clone();
            bytes.extend_from_slice(&self.tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        out.push_str(&format!(
            "\n[... {} bytes discarded; showing the start and the end ...]\n",
            self.discarded()
        ));
        out.push_str(&String::from_utf8_lossy(&self.tail));
        out
    }
}

struct Outcome {
    status: Option<ExitStatus>,
    stdout: Capped,
    stderr: Capped,
    timed_out: bool,
    cancelled: bool,
    duration: Duration,
    /// The readers did not reach EOF within the drain grace after the group was killed.
    drain_incomplete: bool,
}

impl Default for Outcome {
    fn default() -> Self {
        Self {
            status: None,
            stdout: Capped::default(),
            stderr: Capped::default(),
            timed_out: false,
            cancelled: false,
            duration: Duration::ZERO,
            drain_incomplete: false,
        }
    }
}

/// A child in its own process group. The supervisor thread waits for the leader to exit
/// *without* reaping it, kills the whole group while the leader is still a zombie (so the
/// group id cannot have been reused), then reaps and drains. [`Supervised::kill_tree`] takes
/// the same lock, so a timeout or cancellation can only signal a group that is still
/// reserved.
struct Supervised {
    pid: u32,
    reaped: Arc<Mutex<bool>>,
    done: tokio::sync::watch::Receiver<bool>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    _supervisor: thread::JoinHandle<()>,
}

impl Supervised {
    fn spawn(command: &mut Command, cap: usize) -> std::io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child: Child = command.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Each stream gets its own budget: a flooding stdout must not squeeze out the one
        // stderr line that explains the failure.
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = out_tx.send(read_capped(stdout, cap));
        });
        thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr, cap));
        });

        let reaped = Arc::new(Mutex::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let (done_tx, done) = tokio::sync::watch::channel(false);
        let supervisor = thread::Builder::new()
            .name("shell-supervisor".into())
            .spawn({
                let reaped = Arc::clone(&reaped);
                let outcome = Arc::clone(&outcome);
                move || {
                    wait_for_exit_without_reaping(pid);
                    let status = {
                        let mut guard = lock(&reaped);
                        // The leader is a zombie (or still running, if we were asked to
                        // kill): either way its pid and group are still reserved.
                        kill_group(pid);
                        let status = child.wait().ok();
                        *guard = true;
                        status
                    };
                    let (stdout, out_drained) = collect(&out_rx);
                    let (stderr, err_drained) = collect(&err_rx);
                    *outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(Outcome {
                        status,
                        stdout,
                        stderr,
                        drain_incomplete: !(out_drained && err_drained),
                        ..Outcome::default()
                    });
                    let _ = done_tx.send(true);
                }
            })?;

        Ok(Self {
            pid,
            reaped,
            done,
            outcome,
            _supervisor: supervisor,
        })
    }

    /// `true` once the supervisor has reaped the leader and drained the output.
    async fn finished(&mut self) -> bool {
        if *self.done.borrow() {
            return true;
        }
        self.done.changed().await.is_ok()
    }

    /// Kills the group unless the leader has already been reaped.
    fn kill_tree(&self) {
        let guard = lock(&self.reaped);
        if !*guard {
            kill_group(self.pid);
        }
    }

    async fn wait_done(&mut self, grace: Duration) {
        if *self.done.borrow() {
            return;
        }
        let _ = tokio::time::timeout(grace, self.done.changed()).await;
    }

    fn take_outcome(&self) -> Outcome {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default()
    }
}

fn collect(rx: &mpsc::Receiver<Capped>) -> (Capped, bool) {
    match rx.recv_timeout(DRAIN_GRACE) {
        Ok(capped) => (capped, true),
        Err(_) => (Capped::default(), false),
    }
}

/// Reads to EOF, keeping at most `cap` bytes per stream: the first quarter as the head and
/// the last three quarters as the tail, with a count of what fell in between. Reading
/// continues past the cap (and the tail is trimmed as it goes), so a chatty command neither
/// blocks on a full pipe nor grows this process's memory with its output.
fn read_capped(mut reader: impl Read, cap: usize) -> Capped {
    let head_cap = cap / 4;
    let tail_cap = cap - head_cap;
    let mut capped = Capped::default();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                capped.total += n as u64;
                let bytes = &buf[..n];
                let head_take = head_cap.saturating_sub(capped.head.len()).min(n);
                capped.head.extend_from_slice(&bytes[..head_take]);
                let rest = &bytes[head_take..];
                if rest.is_empty() {
                    continue;
                }
                capped.tail.extend_from_slice(rest);
                // Trim in whole-tail chunks: amortized O(1) per byte, and the buffer never
                // exceeds twice the tail cap.
                if tail_cap > 0 && capped.tail.len() > 2 * tail_cap {
                    let drop = capped.tail.len() - tail_cap;
                    capped.tail.drain(..drop);
                }
            }
        }
    }
    if tail_cap > 0 && capped.tail.len() > tail_cap {
        let drop = capped.tail.len() - tail_cap;
        capped.tail.drain(..drop);
    }
    capped
}

fn lock(mutex: &Mutex<bool>) -> MutexGuard<'_, bool> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Blocks until `pid` exits, leaving it unreaped so its pid and process group stay reserved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_exit_without_reaping(pid: u32) {
    let id: libc::id_t = pid;
    loop {
        // SAFETY: `info` is zeroed plain data that waitid fills in; no other preconditions.
        let status = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if status == 0 {
            return;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Platforms without `waitid(WNOWAIT)` have a short window between the reap and the group
/// kill; the tool is verified on macOS and Linux only.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn wait_for_exit_without_reaping(_pid: u32) {}

/// Sends SIGKILL to process group `pid`. Unix only in practice; other platforms have no
/// verified backend for the enforced modes.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(pid) {
            // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

// ---------- reporting ----------

#[allow(clippy::too_many_arguments)]
fn report(
    command: &str,
    workspace: &Path,
    cwd: &Path,
    mode: SandboxMode,
    timeout: Duration,
    clamped: bool,
    output_cap: usize,
    output_clamped: bool,
    outcome: &Outcome,
) -> ToolOutput {
    let stdout = outcome.stdout.render();
    let stderr = outcome.stderr.render();
    let exit_code = outcome.status.and_then(|s| s.code());
    let signal = exit_code.is_none().then(|| status_signal(outcome.status));

    let mut text = String::new();
    text.push_str(&format!("$ {command}\n"));
    if outcome.timed_out {
        text.push_str(&format!(
            "[timed out after {:?}; the process group was killed]\n",
            timeout
        ));
    }
    if outcome.cancelled {
        text.push_str("[cancelled; the process group was killed]\n");
    }
    match exit_code {
        Some(code) => text.push_str(&format!("exit code: {code}\n")),
        None => text.push_str("exit code: none (killed by a signal)\n"),
    }
    text.push_str("--- stdout ---\n");
    text.push_str(&stdout);
    if !stdout.is_empty() && !stdout.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("--- stderr ---\n");
    text.push_str(&stderr);
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        text.push('\n');
    }
    if outcome.stdout.is_truncated() {
        text.push_str(&format!(
            "[stdout truncated: {} of {} bytes discarded]\n",
            outcome.stdout.discarded(),
            outcome.stdout.total
        ));
    }
    if outcome.stderr.is_truncated() {
        text.push_str(&format!(
            "[stderr truncated: {} of {} bytes discarded]\n",
            outcome.stderr.discarded(),
            outcome.stderr.total
        ));
    }
    if outcome.drain_incomplete {
        text.push_str("[output was not fully drained after the kill]\n");
    }
    if clamped {
        text.push_str(&format!(
            "[timeout clamped to the configured maximum of {:?}]\n",
            timeout
        ));
    }
    if output_clamped {
        text.push_str(&format!(
            "[output budget clamped to the configured maximum of {output_cap} bytes per stream]\n"
        ));
    }

    let cwd_relative = cwd
        .strip_prefix(workspace)
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());

    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "command": command,
            "cwd": cwd_relative,
            "sandbox_mode": mode.as_str(),
            "exit_code": exit_code,
            "signal": signal,
            "timed_out": outcome.timed_out,
            "cancelled": outcome.cancelled,
            "timeout_secs": timeout.as_secs(),
            "timeout_clamped": clamped,
            "output_bytes_per_stream": output_cap,
            "output_clamped": output_clamped,
            "stdout_bytes": outcome.stdout.head.len() + outcome.stdout.tail.len(),
            "stderr_bytes": outcome.stderr.head.len() + outcome.stderr.tail.len(),
            "stdout_total_bytes": outcome.stdout.total,
            "stderr_total_bytes": outcome.stderr.total,
            "stdout_truncated_bytes": outcome.stdout.discarded(),
            "stderr_truncated_bytes": outcome.stderr.discarded(),
            "duration_ms": outcome.duration.as_millis() as u64,
        })),
    }
}

#[cfg(unix)]
fn status_signal(status: Option<ExitStatus>) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.and_then(|s| s.signal())
}

#[cfg(not(unix))]
fn status_signal(_status: Option<ExitStatus>) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that yields `total` identical bytes without allocating them.
    struct Repeating {
        remaining: usize,
    }

    impl Read for Repeating {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.remaining);
            buf[..n].fill(b'x');
            self.remaining -= n;
            Ok(n)
        }
    }

    #[test]
    fn the_reader_keeps_a_bounded_head_and_tail() {
        let total = 50 << 20; // 50 MiB
        let cap = 64 << 10;
        let capped = read_capped(Repeating { remaining: total }, cap);

        assert_eq!(capped.total, total as u64);
        assert_eq!(capped.head.len(), cap / 4);
        assert_eq!(capped.tail.len(), cap - cap / 4);
        assert_eq!(capped.discarded(), total as u64 - cap as u64);
        // The retained bytes are bounded by the cap, not by the output size: this is the
        // property that keeps a 50 MiB build log out of this process's memory.
        assert!(capped.head.len() + capped.tail.len() <= cap);
    }

    #[test]
    fn a_short_stream_is_kept_whole() {
        let capped = read_capped(std::io::Cursor::new(b"hello".to_vec()), 64 << 10);
        assert_eq!(capped.total, 5);
        assert_eq!(capped.discarded(), 0);
        assert!(!capped.is_truncated());
        assert_eq!(capped.render(), "hello");
    }

    #[test]
    fn the_last_bytes_survive_when_only_the_tail_fits() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let capped = read_capped(std::io::Cursor::new(data.clone()), 40);

        assert_eq!(capped.head.len(), 10);
        assert_eq!(capped.tail.len(), 30);
        assert_eq!(capped.tail.as_slice(), &data[data.len() - 30..]);
        let rendered = capped.render();
        assert!(rendered.contains("bytes discarded"), "{rendered}");
        let expected_tail = String::from_utf8_lossy(&data[data.len() - 30..]).into_owned();
        assert!(rendered.ends_with(&expected_tail), "{rendered}");
    }
}
