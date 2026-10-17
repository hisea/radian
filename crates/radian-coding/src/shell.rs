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
//! - The command runs in its own **session and process group** (`setsid`), so it has no
//!   controlling terminal and its whole session is killed on exit, timeout or cancellation
//!   ([`signal_session`]: a child that calls `setpgid`, like GNU `timeout`, keeps the session
//!   and is reached even though it left the group). `setsid` rather than `process_group(0)`:
//!   std applies `setpgid(0, 0)` before any `pre_exec` closure runs, and a process-group leader
//!   cannot `setsid` (EPERM). `setsid` already creates a new group, so `killpg(pid, ...)` still
//!   reaches the whole group in one syscall. No controlling terminal also means a sandboxed
//!   command cannot `TIOCSTI` its way into the host terminal (ADR 0016). A command that
//!   deliberately escapes the **session** (its own `setsid`) is out of scope; the drain is
//!   bounded so even that cannot hang the tool.
//! - Output is capped; the model is told how many bytes were discarded.

use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use agent_core::{
    CancellationToken, ContentPart, ToolConcurrency, ToolContext, ToolFailure, ToolOutput, ToolSpec,
};
use radian_sandbox::{Sandbox, SandboxMode};
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::heavy::{HeavyPermit, HeavySlots};
use crate::jobs::{AdoptedHandle, JobCell, Jobs};
use crate::provider::Limits;
use crate::workspace::WorkspaceRoot;

/// How long the supervisor waits for the output readers after the session is killed. The
/// readers end at EOF once every writer is dead; the bound only matters if something escaped
/// the session, and it keeps that from hanging the tool.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// How long the tool waits for the supervisor to reap the killed leader. SIGKILL is
/// immediate; the bound only covers thread scheduling.
const REAP_GRACE: Duration = Duration::from_secs(10);

pub(crate) fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "The shell command; it is passed to `/bin/sh -c`."
            },
            "timeout_secs": {
                "type": "integer",
                "minimum": 1,
                "description": "How long the command may run before its whole session \
                                (every process it started) is \
                                killed. Clamped to the configured maximum. Ignored for \
                                `background: true` unless given, in which case the job is \
                                killed like `job_kill` and settles as timed out. For a \
                                foreground command this deadline is kept from the command's \
                                start even if the command is moved to the background when it \
                                runs past the auto-background threshold."
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
                                the configured maximum. Ignored for `background: true` \
                                (the `job_output` ring is fixed)."
            },
            "background": {
                "type": "boolean",
                "description": "Run the command in the background and return at once \
                                (default false). It keeps running under the same sandbox; \
                                its output goes outside the workspace and is not readable \
                                from inside the sandbox. Read it with `job_output` (pass \
                                `wait: true` to wait for the end), list jobs with \
                                `job_list`, stop one with `job_kill`. `job_output` returns \
                                a bounded incremental ring; use `job_output` with \
                                `tail_lines` / `pattern` / `from_line` to see more than the \
                                last 64 KiB. No default timeout. Do not sleep or poll."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// The `shell` spec for `mode`. The description names the enforcement level, because the
/// model (and a human reading a transcript) must be able to tell a confined run from an
/// unsandboxed one.
///
/// `git_writable` is the sandbox's **effective** answer for this session (ADR 0038 decision 4;
/// [`radian_sandbox::Sandbox::git_writable_except_exec`]), not the policy's request: a general
/// session in the `workspace` tier on macOS can commit with native git, while a task agent (or
/// any backend without the carve-out, Linux) still keeps `.git` read-only, and the description
/// says which. It is ignored outside `workspace` mode.
pub(crate) fn spec(mode: SandboxMode, git_writable: bool) -> ToolSpec {
    let confinement = match mode {
        SandboxMode::WorkspaceWrite if git_writable => {
            "The command runs under the OS sandbox in `workspace` mode: writes are \
             confined to the workspace, the repository's `.git` is writable so native \
             `git commit`, `git branch` and `git stash` work in the sandbox (only the \
             entries git can execute — `.git/config`, `.git/config.worktree` and \
             `.git/hooks/` — stay read-only), sensitive directories such as `~/.ssh` are \
             not readable, and the network is **on** (outbound and local-loopback connect, \
             bind and listen are allowed)."
        }
        SandboxMode::WorkspaceWrite => {
            "The command runs under the OS sandbox in `workspace` mode: writes are \
             confined to the workspace, the repository's `.git` is read-only, sensitive \
             directories such as `~/.ssh` are not readable, and the network is **on** \
             (outbound and local-loopback connect, bind and listen are allowed). \
             `git commit` fails by design (the sandbox keeps `.git` read-only)."
        }
        SandboxMode::Safe => {
            "The command runs under the OS sandbox in `safe` mode: the workspace is \
             read-only and the network is off."
        }
        SandboxMode::FullAccess => {
            "**The command is NOT sandboxed** (`full`): it can read and write \
             anywhere the user can, including the network. No OS sandbox wraps it; that is \
             the built-in default (ADR 0043) and `--profile workspace` turns one on."
        }
    };
    ToolSpec {
        name: "shell".into(),
        description: format!(
            "Run a shell command with `/bin/sh -c`. {confinement} Read files with `read` and \
             `list`, and search with `grep` and `glob`, not with `cat`, `sed -n`, `ls`, `grep` \
             or `find` through `shell`: they report truncation and total size, they use less of \
             the context window, and they run in parallel, while `shell` runs alone. Use \
             `shell` to run programs (builds, tests). Each stream's \
             output is \
             capped: past the budget the start and the end are kept and the middle is \
             replaced by a marker naming the discarded bytes (the end is always kept, \
             because that is where a failing command's error is). The whole session is \
             killed on timeout or cancellation. The timeout is clamped to the configured \
             maximum (600 s by default); for a command that needs longer use `background: \
             true`, which has no default timeout. A foreground command still running after the \
             configured auto-background threshold (120 s by default) is moved to the \
             background **in place** — the same process, never restarted — and this call \
             returns at once with a job id, its output file, and the output so far. Use `cwd` \
             (workspace-relative) to run \
             somewhere other than the workspace root. Set `background: true` for a command \
             that outlives the foreground timeout: it returns at once with a job id, and the \
             three `job_*` tools read, list and stop it. `job_output` is a bounded \
             incremental ring; use `job_output` with `tail_lines` / `pattern` / `from_line` to \
             see more than the last 64 KiB. Background jobs \
             have no default timeout. Do not sleep or poll; use `job_output` with `wait: \
             true` to wait for the end."
        ),
        input_schema: schema(),
        // A shell command has side effects and is not safe to overlap with other calls.
        concurrency: ToolConcurrency::Exclusive,
    }
}

/// The result of a user's `!`/`!!` command (ADR 0033), from the same sandboxed executor the
/// `shell` tool uses. `stdout`/`stderr` already carry the truncation marker when capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserBash {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Runs a user's `!`/`!!` command through the **same sandboxed executor** as the `shell` tool
/// (ADR 0033): the workspace root as cwd, the default timeout and per-stream output caps, the
/// same process-group supervision, the same cancellation. It never builds a naked `Command`:
/// the caller passes the sandbox it already built, and a `safe` tier (no sandbox) cannot reach
/// here (the caller refuses it before). The child gets no controlling terminal (`setsid`), so
/// ADR 0016's `TIOCSTI` rule is unchanged.
pub async fn run_user_bash(
    root: &Path,
    sandbox: &Sandbox,
    command: &str,
    cancel: &CancellationToken,
) -> Result<UserBash, String> {
    let limits = Limits::default();
    let workspace = WorkspaceRoot::new(root).map_err(|error| error.to_string())?;
    let outcome = execute(
        sandbox,
        workspace.root(),
        command,
        limits.default_shell_timeout,
        limits.default_shell_output_bytes,
        cancel,
        // ADR 0037: `!`/`!!` never turn into a background job.
        None,
    )
    .await
    .map_err(|failure| failure.message)?;
    // `execute` cannot adopt without an `Adopt` specification.
    let ExecOutcome::Ran(outcome) = outcome else {
        unreachable!("a user `!` command never adopts a job");
    };
    Ok(UserBash {
        stdout: outcome.stdout.render(),
        stderr: outcome.stderr.render(),
        exit_code: outcome.status.and_then(|status| status.code()),
        signal: status_signal(outcome.status),
        timed_out: outcome.timed_out,
        cancelled: outcome.cancelled,
    })
}

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
    timeout_secs: Option<u64>,
    cwd: Option<String>,
    /// Per-stream output budget; clamped to the hard cap.
    max_output_bytes: Option<usize>,
    /// ADR 0037: run in the background and return at once.
    background: Option<bool>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    root: &WorkspaceRoot,
    sandbox: &Sandbox,
    limits: &Limits,
    jobs: &Jobs,
    heavy: &HeavySlots,
    arguments: &serde_json::Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let args: ShellArgs = args::parse("shell", &schema(), arguments)?;
    if args.command.trim().is_empty() {
        return Err(args::invalid(
            "shell",
            arguments,
            ["`command` must not be empty"],
        ));
    }
    // The cwd check is the same workspace boundary the file tools use (ADR 0039 decision 2:
    // walked by fd, no symlink in any component, must be a directory) before anything is
    // spawned; the sandbox policy is what confines the child itself.
    let cwd = match args.cwd.as_deref().filter(|dir| !dir.is_empty()) {
        Some(dir) => root.checked_dir_path(dir)?,
        None => root.root().to_path_buf(),
    };

    // ADR 0030 O8: a command matching a heavy pattern takes a process-level slot before it runs.
    // The wait respects the turn's cancellation and takes no slot when cancelled.
    let permit = match heavy.acquire(&args.command, &cx.cancellation).await {
        Ok(permit) => permit,
        Err(_) => return Ok(cancelled_waiting_for_slot(&args.command)),
    };

    if args.background.unwrap_or(false) {
        return start_background(root, sandbox, jobs, &cwd, &args, permit);
    }

    let (deadline, clamped) = clamp_timeout(args.timeout_secs, limits);
    let (output_cap, output_clamped) = clamp_output(args.max_output_bytes, limits);

    // ADR 0037: a foreground command still running after `auto_background_after`
    // is adopted by the job registry in place. The foreground wait is the threshold itself
    // when the model gave no `timeout_secs` (the threshold is what replaces the default cap:
    // there is no foreground cap after adoption); with an explicit `timeout_secs` the
    // deadline is that value **from the command's start**, and adoption only happens when
    // the threshold falls before it (the job is then killed at the original deadline).
    let auto = limits
        .auto_background_after
        .filter(|after| !after.is_zero());
    let (wait, adopt_threshold, explicit_deadline) = match auto {
        Some(after) if args.timeout_secs.is_none() => (after, Some(after), None),
        Some(after) if after < deadline => (after, Some(after), Some(deadline)),
        _ => (deadline, None, None),
    };

    // The heavy slot is held while the command runs (ADR 0030 O8). `queued` is read before the
    // permit may move into an adopted job; when the command runs to completion the permit drops
    // here, and when it is adopted the job holds it for its whole life.
    let queued = permit.as_ref().map(HeavyPermit::waited);
    let mut permit = permit;
    let outcome = execute(
        sandbox,
        &cwd,
        &args.command,
        wait,
        output_cap,
        &cx.cancellation,
        adopt_threshold.map(|_| Adopt {
            jobs,
            command: &args.command,
            explicit_deadline,
            heavy: permit.take(),
        }),
    )
    .await?;
    match outcome {
        ExecOutcome::Ran(outcome) => Ok(report(
            &args.command,
            root.root(),
            &cwd,
            sandbox.mode(),
            explicit_deadline.unwrap_or(wait),
            clamped,
            output_cap,
            output_clamped,
            queued,
            &outcome,
        )),
        ExecOutcome::Adopted(info) => Ok(adopted_report(
            &args.command,
            root.root(),
            &cwd,
            sandbox.mode(),
            wait,
            clamped,
            output_cap,
            output_clamped,
            queued,
            &info,
        )),
    }
}

/// The result when a heavy command's wait for a slot was cancelled before it ran (ADR 0030
/// O8): the command did not run, and the result says so. It mirrors the cancelled shape of a
/// running command so a host renders both the same way.
fn cancelled_waiting_for_slot(command: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentPart::text(format!(
            "$ {command}\n[cancelled while waiting for a heavy slot; the command did not run]\n"
        ))],
        structured: Some(json!({
            "command": command,
            "cancelled": true,
            "heavy_queued": true,
        })),
    }
}

/// Starts `command` as a background job (ADR 0037) and returns the one-line acknowledgement:
/// the job id, the `job_output` hint and the three tools. No default timeout; an explicit
/// `timeout_secs` kills the job like `job_kill` and settles it as timed out.
///
/// A heavy command arrives with `heavy` already held (ADR 0030 O8): the job keeps the slot for
/// its whole life and drops it on settle, so background heavy jobs queue behind foreground ones
/// and each other. `job_kill` never consults the gate, so a held slot cannot block a kill.
fn start_background(
    root: &WorkspaceRoot,
    sandbox: &Sandbox,
    jobs: &Jobs,
    cwd: &Path,
    args: &ShellArgs,
    heavy: Option<HeavyPermit>,
) -> Result<ToolOutput, ToolFailure> {
    let timeout = args
        .timeout_secs
        .map(|secs| Duration::from_secs(secs.max(1)));
    let queued = heavy.as_ref().map(HeavyPermit::waited);
    let info = jobs.start(sandbox, cwd, &args.command, timeout, heavy)?;
    let cwd_relative = cwd
        .strip_prefix(root.root())
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    let mut text = format!(
        "started background job {id}\n\
         its output is {output}\n\
         read it with `job_output` (pass `wait: true` to wait for the end); `job_list` lists \
         this session's jobs; `job_kill` stops one.\n\
         `job_output` is a bounded incremental ring; use `job_output` with `tail_lines`, \
         `pattern` or `from_line`/`max_lines` to see the whole file.\n\
         Do not sleep or poll: use `job_output` with `wait: true` to wait for the end.\n",
        id = info.id,
        output = info.output,
    );
    if let Some(queued) = queued {
        text.push_str(&format!(
            "[queued for a heavy slot: waited {} ms before starting]\n",
            queued.as_millis()
        ));
    }
    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "background": true,
            "job_id": info.id,
            "output": info.output,
            "command": args.command,
            "cwd": cwd_relative,
            "sandbox_mode": sandbox.mode().as_str(),
            "timeout_secs": timeout.map(|t| t.as_secs()),
            "heavy_queued_ms": queued.map(|queued| queued.as_millis() as u64),
        })),
    })
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

/// The sandboxed `/bin/sh -c` command for `command`, with the cwd set and (for a transparent
/// backend) the environment filtered to the policy whitelist. Shared by the foreground
/// executor and background jobs (ADR 0037): a job runs under exactly the same sandbox and
/// environment rules as a foreground command.
pub(crate) fn sandboxed_command(
    sandbox: &Sandbox,
    cwd: &Path,
    command: &str,
) -> Result<Command, ToolFailure> {
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
    Ok(process)
}

/// What a foreground [`execute`] produced (ADR 0037): the finished outcome, or the
/// job the still-running call was adopted into.
pub(crate) enum ExecOutcome {
    Ran(Outcome),
    Adopted(crate::jobs::AdoptInfo),
}

/// The adoption parameters for a foreground [`execute`] (ADR 0037). Present only
/// when `auto_background_after` is on and the threshold can be reached before the foreground
/// deadline; `None` keeps the pre-revision behaviour.
struct Adopt<'a> {
    jobs: &'a Jobs,
    command: &'a str,
    /// The explicit `timeout_secs` from the command's start, if any: the adopted job is
    /// killed then, not at the threshold.
    explicit_deadline: Option<Duration>,
    /// The heavy slot the command already holds (ADR 0030 O8): an adopted job takes it on, so
    /// the slot is held for the job's whole life and released when it settles.
    heavy: Option<HeavyPermit>,
}

async fn execute(
    sandbox: &Sandbox,
    cwd: &Path,
    command: &str,
    timeout: Duration,
    output_cap: usize,
    cancel: &CancellationToken,
    adopt: Option<Adopt<'_>>,
) -> Result<ExecOutcome, ToolFailure> {
    let mut process = sandboxed_command(sandbox, cwd, command)?;

    let started = Instant::now();
    let mut supervised = Supervised::spawn(&mut process, output_cap).map_err(|e| {
        fail(
            "shell_spawn_failed",
            format!("could not start the command: {e}"),
        )
    })?;

    let mut timed_out = false;
    let mut cancelled = false;
    let mut slots_full = false;

    // Phase 1: wait for the end, the foreground deadline (the adoption threshold when an
    // `Adopt` is active), or the turn's cancellation.
    let mut timer_fired = false;
    let mut kill = tokio::select! {
        finished = supervised.finished() => !finished,
        () = tokio::time::sleep(timeout) => {
            timer_fired = true;
            false
        }
        () = cancel.cancelled() => {
            cancelled = true;
            true
        }
    };

    if !kill && timer_fired {
        match adopt {
            None => {
                timed_out = true;
                kill = true;
            }
            Some(mut spec) => {
                let remaining = spec
                    .explicit_deadline
                    .map(|deadline| deadline.saturating_sub(started.elapsed()));
                // ADR 0030 O8: the adopted job takes the heavy slot on (the same permit), so it
                // is held for the job's whole life and released when the job settles.
                let heavy = spec.heavy.take();
                match spec
                    .jobs
                    .adopt(&supervised, spec.command, started, remaining, heavy)
                {
                    Ok(info) => return Ok(ExecOutcome::Adopted(info)),
                    // No free slot (ADR 0037): do not adopt. The call
                    // keeps running to its foreground deadline and is killed there.
                    Err(_) => {
                        slots_full = true;
                        let rest = remaining.unwrap_or(Duration::ZERO);
                        if rest.is_zero() {
                            timed_out = true;
                            kill = true;
                        } else {
                            let mut rest_fired = false;
                            let rest_kill = tokio::select! {
                                finished = supervised.finished() => !finished,
                                () = tokio::time::sleep(rest) => {
                                    rest_fired = true;
                                    false
                                }
                                () = cancel.cancelled() => {
                                    cancelled = true;
                                    true
                                }
                            };
                            if rest_fired {
                                timed_out = true;
                            }
                            kill = rest_kill || rest_fired;
                        }
                    }
                }
            }
        }
    }

    if kill {
        supervised.kill_tree();
    }
    supervised.wait_done(REAP_GRACE).await;

    let mut outcome = supervised.take_outcome();
    outcome.timed_out = timed_out;
    outcome.cancelled = cancelled;
    outcome.slots_full = slots_full;
    outcome.duration = started.elapsed();
    Ok(ExecOutcome::Ran(outcome))
}

// ---------- supervision ----------

#[derive(Default)]
struct Capped {
    /// The first quarter of the budget, kept verbatim.
    head_cap: usize,
    /// The last three quarters, kept as a ring.
    tail_cap: usize,
    /// The first part of the stream, kept verbatim.
    head: Vec<u8>,
    /// The last part of the stream. A ring in spirit: it is trimmed as it grows, so memory
    /// never depends on how much the command printed.
    tail: Vec<u8>,
    /// Every byte this stream produced, kept or not.
    total: u64,
}

impl Capped {
    /// A capture with `cap` bytes of budget: the first quarter as the head, the last three
    /// quarters as the tail.
    fn with_cap(cap: usize) -> Self {
        let head_cap = cap / 4;
        Self {
            head_cap,
            tail_cap: cap - head_cap,
            ..Self::default()
        }
    }

    /// Adds one chunk, trimming the tail as it grows (amortized O(1) per byte; the buffer
    /// never exceeds twice the tail cap).
    fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let head_take = self
            .head_cap
            .saturating_sub(self.head.len())
            .min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_take]);
        let rest = &bytes[head_take..];
        if rest.is_empty() {
            return;
        }
        self.tail.extend_from_slice(rest);
        if self.tail_cap > 0 && self.tail.len() > 2 * self.tail_cap {
            let drop = self.tail.len() - self.tail_cap;
            self.tail.drain(..drop);
        }
    }

    /// Trims the tail to its cap. Called once at end of stream (and before a promotion), so a
    /// small tail is exactly the retained budget rather than up to twice it.
    fn trim(&mut self) {
        if self.tail_cap > 0 && self.tail.len() > self.tail_cap {
            let drop = self.tail.len() - self.tail_cap;
            self.tail.drain(..drop);
        }
    }

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
        out.push_str(&self.marker());
        out.push_str(&String::from_utf8_lossy(&self.tail));
        out
    }

    /// The marker that names the discarded middle, on its own line.
    fn marker(&self) -> String {
        format!(
            "\n[... {} bytes discarded; showing the start and the end ...]\n",
            self.discarded()
        )
    }

    /// Writes the retained bytes (head, marker if the middle was dropped, then tail) to `out`.
    /// Used to seed a job's output file when a foreground command is adopted (ADR 0037
    /// revision 1). Returns the number of bytes written.
    fn write_to(&self, out: &mut impl Write) -> std::io::Result<u64> {
        out.write_all(&self.head)?;
        let mut written = self.head.len() as u64;
        if self.is_truncated() {
            let marker = self.marker();
            out.write_all(marker.as_bytes())?;
            written += marker.len() as u64;
        }
        out.write_all(&self.tail)?;
        Ok(written + self.tail.len() as u64)
    }
}

/// One output stream's destination (ADR 0037): the foreground executor's bounded
/// head+tail capture, until [`StreamSink::promote`] switches it — under this lock, at
/// adoption — to the job's `.out` file plus the [`JobCell`] ring.
///
/// A reader takes the lock once per chunk, and `promote` takes it too, so a promotion is
/// atomic with respect to one chunk: no byte is lost, and none is written twice.
pub(crate) struct StreamSink {
    capped: Capped,
    job: Option<JobSink>,
}

struct JobSink {
    file: File,
    cell: Arc<Mutex<JobCell>>,
}

impl StreamSink {
    fn new(cap: usize) -> Self {
        Self {
            capped: Capped::with_cap(cap),
            job: None,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        match &mut self.job {
            None => self.capped.push(bytes),
            Some(job) => {
                let _ = job.file.write_all(bytes);
                let mut cell = lock(&job.cell);
                cell.push_ring(bytes);
            }
        }
    }

    /// Flushes the retained pre-adoption output into `file`, then switches to job mode.
    /// Returns `(bytes written to the file, the rendered pre-adoption output)`. The caller
    /// holds this sink's lock; the file is written before the sink can accept more bytes.
    pub(crate) fn promote(&mut self, mut file: File, cell: Arc<Mutex<JobCell>>) -> (u64, String) {
        self.capped.trim();
        let rendered = self.capped.render();
        let written = self.capped.write_to(&mut file).unwrap_or(0);
        self.capped = Capped::default();
        self.job = Some(JobSink { file, cell });
        (written, rendered)
    }

    /// Takes the captured head+tail at end of stream. Empty for a stream that was promoted
    /// (its bytes went to the job instead). Trimmed to the exact budget, so the reported size
    /// is the budget and not up to twice it.
    fn take_capped(&mut self) -> Capped {
        let mut capped = std::mem::take(&mut self.capped);
        capped.trim();
        capped
    }
}

pub(crate) struct Outcome {
    pub(crate) status: Option<ExitStatus>,
    stdout: Capped,
    stderr: Capped,
    timed_out: bool,
    cancelled: bool,
    /// The call was not adopted because all job slots were busy (ADR 0037
    /// decision 4).
    slots_full: bool,
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
            slots_full: false,
            duration: Duration::ZERO,
            drain_incomplete: false,
        }
    }
}

/// A child in its own process group. The supervisor thread waits for the leader to exit
/// *without* reaping it, kills its whole session while the leader is still a zombie (so the
/// session id cannot have been reused), then reaps and drains. [`Supervised::kill_tree`] takes
/// the same lock, so a timeout or cancellation can only signal a session that is still
/// reserved.
///
/// A foreground command adopted by the job registry (ADR 0037) keeps this same
/// child and this same supervisor: `Jobs::adopt` installs an [`AdoptedHandle`] into
/// `adopted`, and the supervisor settles the job after it reaps. Nothing is restarted.
pub(crate) struct Supervised {
    pub(crate) pid: u32,
    pub(crate) reaped: Arc<Mutex<bool>>,
    pub(crate) done: tokio::sync::watch::Receiver<bool>,
    pub(crate) outcome: Arc<Mutex<Option<Outcome>>>,
    /// The foreground capture of each stream, promotable to a job's file+ring.
    pub(crate) stdout: Arc<Mutex<StreamSink>>,
    pub(crate) stderr: Arc<Mutex<StreamSink>>,
    /// The job this command was adopted into, if any. The supervisor takes it once and
    /// settles; `Jobs::adopt` takes it too if the supervisor already finished (exactly one of
    /// the two wins the take).
    pub(crate) adopted: Arc<Mutex<Option<AdoptedHandle>>>,
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
            // `setsid` makes the child a session leader with a new process group and **no
            // controlling terminal**. It replaces `process_group(0)`: std runs `setpgid(0, 0)`
            // before the `pre_exec` closures, and a process-group leader cannot `setsid`
            // (EPERM). `setsid` already creates the group, so `killpg(pid, ...)` still reaches
            // the whole tree. No controlling terminal is also what stops `TIOCSTI` into the
            // host terminal (ADR 0016).
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child: Child = command.spawn()?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // Each stream gets its own budget: a flooding stdout must not squeeze out the one
        // stderr line that explains the failure. Each stream also owns a sink so it can be
        // promoted to a job in place (ADR 0037).
        let stdout_sink = Arc::new(Mutex::new(StreamSink::new(cap)));
        let stderr_sink = Arc::new(Mutex::new(StreamSink::new(cap)));
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        {
            let sink = Arc::clone(&stdout_sink);
            thread::spawn(move || {
                read_stream(stdout, Arc::clone(&sink));
                let _ = out_tx.send(lock(&sink).take_capped());
            });
        }
        {
            let sink = Arc::clone(&stderr_sink);
            thread::spawn(move || {
                read_stream(stderr, Arc::clone(&sink));
                let _ = err_tx.send(lock(&sink).take_capped());
            });
        }

        let reaped = Arc::new(Mutex::new(false));
        let outcome = Arc::new(Mutex::new(None));
        let adopted: Arc<Mutex<Option<AdoptedHandle>>> = Arc::new(Mutex::new(None));
        let (done_tx, done) = tokio::sync::watch::channel(false);
        let supervisor = thread::Builder::new()
            .name("shell-supervisor".into())
            .spawn({
                let reaped = Arc::clone(&reaped);
                let outcome = Arc::clone(&outcome);
                let adopted = Arc::clone(&adopted);
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
                    // Publish `done` **before** taking the adopted handle: `Jobs::adopt`'s
                    // fallback only takes the handle when `done` is already set, so exactly
                    // one side ever settles (ADR 0037).
                    let _ = done_tx.send(true);
                    let handle = adopted.lock().unwrap_or_else(|e| e.into_inner()).take();
                    if let Some(handle) = handle {
                        handle.settle(status);
                    }
                }
            })?;

        Ok(Self {
            pid,
            reaped,
            done,
            outcome,
            stdout: stdout_sink,
            stderr: stderr_sink,
            adopted,
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

    /// Kills the whole session unless the leader has already been reaped.
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

/// Reads to EOF, keeping at most `cap` bytes: the first quarter as the head and the last
/// three quarters as the tail, with a count of what fell in between. Reading continues past
/// the cap (and the tail is trimmed as it goes), so a chatty command neither blocks on a full
/// pipe nor grows this process's memory with its output.
///
/// The tool itself reads through [`StreamSink`] (so a running command can be adopted in
/// place); this is the plain capture the reader tests pin.
#[cfg(test)]
fn read_capped(mut reader: impl Read, cap: usize) -> Capped {
    let mut capped = Capped::with_cap(cap);
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => capped.push(&buf[..n]),
        }
    }
    capped.trim();
    capped
}

/// Reads a stream to EOF, handing each chunk to the shared sink. The sink decides where the
/// bytes go (the foreground capture, or the job's file and ring after adoption), so a running
/// command can be promoted without restarting its readers (ADR 0037).
fn read_stream(mut reader: impl Read, sink: Arc<Mutex<StreamSink>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => lock(&sink).push(&buf[..n]),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Locks a `Mutex<bool>` from another module (background jobs share the `reaped` invariant).
pub(crate) fn lock_bool(mutex: &Mutex<bool>) -> MutexGuard<'_, bool> {
    lock(mutex)
}

/// Blocks until `pid` exits, leaving it unreaped so its pid and process group stay reserved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn wait_for_exit_without_reaping(pid: u32) {
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
pub(crate) fn wait_for_exit_without_reaping(_pid: u32) {}

/// Sends `signal` to process group `pid`. Unix only in practice; other platforms have no
/// verified backend for the enforced modes.
pub(crate) fn signal_group(pid: u32, signal: i32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(pid) {
            // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
            unsafe {
                libc::killpg(pgid, signal);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
    }
}

/// Sends `signal` to **every process in session `sid`**: the process group `sid` first (one
/// syscall, and it also reaches a member that appears after the enumeration), then every process
/// the enumeration finds in that session.
///
/// A child that calls `setpgid` leaves the process group but **keeps the session** — GNU
/// `timeout` does exactly that, and so does a shell's own job control — so a group-only signal
/// leaves it running, reparented to pid 1 (dogfood bug 2026-10-17: `timeout -k 30 2400 cargo
/// test` outlived the kill of the job's group). A child that calls `setsid` itself leaves the
/// session as well; no session-scoped signal can reach it, and that is out of scope.
///
/// The caller holds the leader's `reaped` flag (see [`crate::jobs`]): while the leader is
/// unreaped its pid stays reserved, and a session id is only ever created from its own leader's
/// pid, so every process found here is the job's. A member that exits between the enumeration and
/// its `kill` can only turn that `kill` into a no-op; the window is a few microseconds and is the
/// same one `killpg` has.
pub(crate) fn signal_session(sid: u32, signal: i32) {
    #[cfg(unix)]
    {
        signal_group(sid, signal);
        for pid in session_members(sid) {
            // The leader is in group `sid` and was signalled above.
            if pid == sid {
                continue;
            }
            if let Ok(pid) = libc::pid_t::try_from(pid) {
                // SAFETY: `kill` takes plain integers and has no memory-safety preconditions.
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (sid, signal);
    }
}

/// Every process whose session id is `sid`. macOS asks libproc and `getsid(2)`; Linux reads
/// `/proc/<pid>/stat`; a platform with no enumeration returns nothing, and [`signal_session`]'s
/// group signal is then all there is.
#[cfg(target_os = "macos")]
fn session_members(sid: u32) -> Vec<u32> {
    use std::mem::size_of;
    let mut out = Vec::new();
    // `proc_listallpids(NULL, 0)` returns the buffer size in bytes; with a buffer it returns the
    // number of pids it wrote into it (measured on macOS 27).
    // SAFETY: the size query writes nothing; the fill call is handed the buffer and its length.
    let needed = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return out;
    }
    let mut pids = vec![0 as libc::c_int; needed as usize / size_of::<libc::c_int>() + 64];
    let written = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if written <= 0 {
        return out;
    }
    // `written` is a pid count; clamping keeps the slice in bounds even if an SDK counted
    // something else, and a zero entry is skipped either way.
    for &pid in &pids[..(written as usize).min(pids.len())] {
        if pid > 0 && process_session(pid as u32) == Some(sid) {
            out.push(pid as u32);
        }
    }
    out
}

/// `getsid(2)`: the session a live process is in, or `None` if it is gone. Measured on macOS 27:
/// it answers for a process in **another** session, with no privilege (BSD does not enforce
/// POSIX's `EPERM` for a different session).
#[cfg(target_os = "macos")]
fn process_session(pid: u32) -> Option<u32> {
    let pid = libc::pid_t::try_from(pid).ok()?;
    // SAFETY: `getsid` takes a pid and has no memory-safety preconditions.
    let sid = unsafe { libc::getsid(pid) };
    (sid >= 0).then_some(sid as u32)
}

/// Every process whose session id is `sid`, from `/proc`. Unverified here: Linux cannot be run on
/// this machine (the code only has to keep compiling for it).
#[cfg(target_os = "linux")]
fn session_members(sid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if session_of_stat(pid) == Some(sid) {
            out.push(pid);
        }
    }
    out
}

/// Field 6 of `/proc/<pid>/stat`, the session. `comm` (field 2) may hold spaces and parentheses,
/// so everything up to the **last** `)` is skipped; after it the fields are
/// `state ppid pgrp session`.
#[cfg(target_os = "linux")]
fn session_of_stat(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn session_members(_sid: u32) -> Vec<u32> {
    Vec::new()
}

/// Sends SIGKILL to every process of session `pid`. Unix only in practice; other platforms have
/// no verified backend for the enforced modes.
fn kill_group(pid: u32) {
    #[cfg(unix)]
    signal_session(pid, libc::SIGKILL);
    #[cfg(not(unix))]
    let _ = pid;
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
    queued: Option<Duration>,
    outcome: &Outcome,
) -> ToolOutput {
    let stdout = outcome.stdout.render();
    let stderr = outcome.stderr.render();
    let exit_code = outcome.status.and_then(|s| s.code());
    let signal = exit_code.is_none().then(|| status_signal(outcome.status));

    let mut text = String::new();
    text.push_str(&format!("$ {command}\n"));
    // ADR 0030 O8: say the command queued for a heavy slot, and for how long.
    if let Some(queued) = queued {
        text.push_str(&format!(
            "[queued for a heavy slot: waited {} ms before running]\n",
            queued.as_millis()
        ));
    }
    if outcome.timed_out {
        text.push_str(&format!(
            "[timed out after {timeout:?}; the whole session was killed. For a command that \
             needs longer, use `background: true` (no default timeout) and `job_output` with \
             `wait: true`.\n"
        ));
    }
    if outcome.cancelled {
        text.push_str("[cancelled; the whole session was killed]\n");
    }
    if outcome.slots_full {
        text.push_str(
            "[all background job slots were busy, so this call was **not** moved to the \
             background; it ran to its foreground timeout. Finish a job with `job_output \
             wait: true`, stop one with `job_kill`, or list them with `job_list`.]\n",
        );
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
            "[timeout clamped to the configured maximum of {timeout:?}]\n"
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
            "slots_full": outcome.slots_full,
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
            "heavy_queued_ms": queued.map(|queued| queued.as_millis() as u64),
            "duration_ms": outcome.duration.as_millis() as u64,
        })),
    }
}

/// The result of a foreground call that was adopted by the job registry in place (ADR 0037
/// revision 1): the job id, the `job_output` hint, and a bounded tail of the output produced
/// before adoption. The command is **not** restarted, and its output file already holds that
/// pre-adoption output.
#[allow(clippy::too_many_arguments)]
fn adopted_report(
    command: &str,
    workspace: &Path,
    cwd: &Path,
    mode: SandboxMode,
    threshold: Duration,
    clamped: bool,
    output_cap: usize,
    output_clamped: bool,
    queued: Option<Duration>,
    info: &crate::jobs::AdoptInfo,
) -> ToolOutput {
    let mut text = String::new();
    text.push_str(&format!("$ {command}\n"));
    // ADR 0030 O8: the heavy wait happened before the command started; the adopted job holds
    // the slot on for its whole life (the permit moved into the job).
    if let Some(queued) = queued {
        text.push_str(&format!(
            "[queued for a heavy slot: waited {} ms before starting]\n",
            queued.as_millis()
        ));
    }
    text.push_str(&format!(
        "[still running after {threshold:?}; moved to the background as job {id} — the same \
         process, not restarted. Its output is {output}. Read it with `job_output` (pass \
         `wait: true` to wait for the end); `job_list` lists this session's jobs and \
         `job_kill` stops one. Do not sleep or poll.]\n",
        id = info.id,
        output = info.output,
    ));
    text.push_str("--- stdout so far ---\n");
    text.push_str(&info.stdout);
    if !info.stdout.is_empty() && !info.stdout.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("--- stderr so far ---\n");
    text.push_str(&info.stderr);
    if !info.stderr.is_empty() && !info.stderr.ends_with('\n') {
        text.push('\n');
    }
    if clamped {
        text.push_str(&format!(
            "[timeout clamped to the configured maximum of {threshold:?}]\n"
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
            "background": true,
            "adopted": true,
            "job_id": info.id,
            "output": info.output,
            "command": command,
            "cwd": cwd_relative,
            "sandbox_mode": mode.as_str(),
            "auto_background_secs": threshold.as_secs(),
            "adopted_bytes": info.adopted_bytes,
            "heavy_queued_ms": queued.map(|queued| queued.as_millis() as u64),
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
