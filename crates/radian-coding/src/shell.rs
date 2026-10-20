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
use crate::foreground::{ForegroundOutputs, Retention};
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
                "description": "How long a foreground command may run before its whole \
                                session is killed. Clamped to the configured maximum. Ignored \
                                for `background: true` (no deadline: bound the command itself \
                                or stop it with `job`)."
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
                "description": "Output budget per stream. Past it the start and the end \
                                are kept and the middle is replaced by a marker naming the \
                                discarded bytes. Clamped to the configured maximum."
            },
            "background": {
                "type": "boolean",
                "description": "Run in the background and return at once with a job id \
                                (default false). No default timeout. Read it with `job`."
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
/// session in the `workspace` tier on macOS can commit with native git, while any backend
/// without the carve-out (Linux) still keeps `.git` read-only, and the description says which.
/// It is ignored outside `workspace` mode.
pub(crate) fn spec(mode: SandboxMode, git_writable: bool) -> ToolSpec {
    let confinement = match mode {
        SandboxMode::WorkspaceWrite if git_writable => {
            "The command runs under the OS sandbox in `workspace` mode: writes are \
             confined to the workspace, the repository's `.git` is writable so native \
             `git commit` works (only `.git/config`, `.git/config.worktree` and \
             `.git/hooks/` stay read-only), `~/.ssh` is not readable, and the network is \
             **on**."
        }
        SandboxMode::WorkspaceWrite => {
            "The command runs under the OS sandbox in `workspace` mode: writes are \
             confined to the workspace, the repository's `.git` is read-only (so is \
             `git commit`, by design), `~/.ssh` is not readable, and the network is **on**."
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
            "Run a shell command with `/bin/sh -c`. {confinement} Read files with `read` \
             and `list`, and search with `grep` and `glob`, not with `cat`, `sed`, `ls` or \
             `find` through `shell`: they report truncation, use less context, and run in \
             parallel, while `shell` runs alone. Each stream is capped: past the \
             budget the start and the end are kept and the middle names the discarded \
             bytes. A truncated foreground result keeps its full output under a job id for \
             `job`. Timeout or cancellation kills the whole session. A \
             foreground command still running after the auto-background threshold is moved \
             to the background in place — never restarted — and returns at once with a job \
             id. `background: true` starts it there at once. Background results end: `You \
             will be notified when it finishes. Do not sleep or poll. If you have nothing \
             else to do, end your reply.`"
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
        // A user `!` command's output is shown and forgotten; nothing is retained for
        // `shell_output`.
        None,
    )
    .await
    .map_err(|failure| failure.message)?;
    // `execute` cannot adopt without an `Adopt` specification.
    let ExecOutcome::Ran(outcome) = outcome else {
        unreachable!("a user `!` command never adopts a job");
    };
    Ok(UserBash {
        stdout: outcome.stdout.render_for(outcome.succeeded()),
        stderr: outcome.stderr.render_for(outcome.succeeded()),
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
    foreground: Option<&ForegroundOutputs>,
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

    if args.background.unwrap_or(false) {
        return start_background(root, sandbox, jobs, &cwd, &args);
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

    // Retain the whole output so a truncated result's discarded middle stays retrievable. `None`
    // when the provider has no caller-supplied private directory (no-hand embedders) or the
    // private root is unusable: the command runs and its result simply has no job id. The file
    // is removed again below (or by `Retention`'s drop on an error path) when it is not kept.
    let retention = match foreground {
        Some(foreground) => foreground.open(&args.command),
        None => None,
    };

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
        }),
        // The whole output is teed to a session-private file so a truncated result's discarded
        // middle stays retrievable; nothing is created for a result that needs no handle, and
        // the file is removed again below.
        retention.clone(),
    )
    .await?;
    match outcome {
        ExecOutcome::Ran(outcome) => {
            // The exit status is known only now, so the view (success 1/4:3/4, failure
            // 1/8:7/8) is picked here; the capture kept the maximum of both.
            let success = outcome.succeeded();
            let truncated = outcome.stdout.is_truncated_for(success)
                || outcome.stderr.is_truncated_for(success);
            let retained = match (foreground, &retention) {
                (Some(foreground), Some(retention)) if truncated => {
                    let state = if outcome.timed_out {
                        crate::jobs::JobState::TimedOut
                    } else {
                        crate::jobs::exit_state(outcome.status)
                    };
                    let view = RetainedView {
                        job_id: retention.id().to_owned(),
                        retained_bytes: retention.retained(),
                        total_bytes: retention.total(),
                        cut: retention.is_cut(),
                    };
                    foreground.finish(retention, Some(state));
                    Some(view)
                }
                (Some(foreground), Some(retention)) => {
                    // No truncation: no entry, no artifact. The file is removed.
                    foreground.finish(retention, None);
                    None
                }
                _ => None,
            };
            Ok(report(
                &args.command,
                root.root(),
                &cwd,
                sandbox.mode(),
                explicit_deadline.unwrap_or(wait),
                clamped,
                output_cap,
                output_clamped,
                &outcome,
                retained,
            ))
        }
        ExecOutcome::Adopted(info) => {
            // The job's own output file is the retained output now; the foreground file is
            // redundant. Drop it (the pre-adoption bytes already reached the job's file).
            if let (Some(foreground), Some(retention)) = (foreground, &retention) {
                foreground.finish(retention, None);
            }
            Ok(adopted_report(
                &args.command,
                root.root(),
                &cwd,
                sandbox.mode(),
                wait,
                clamped,
                output_cap,
                output_clamped,
                &info,
            ))
        }
    }
}

/// Starts `command` as a background job (ADR 0037) and returns the one-line acknowledgement:
/// the job id and the `job` hint. No timeout: a background job has no deadline, and an
/// explicit `timeout_secs` is **ignored** (ADR 0037). The acknowledgement says so
/// and names the two ways to bound the job — `timeout N …` inside the command, or `job`
/// (`op: "kill"`).
fn start_background(
    root: &WorkspaceRoot,
    sandbox: &Sandbox,
    jobs: &Jobs,
    cwd: &Path,
    args: &ShellArgs,
) -> Result<ToolOutput, ToolFailure> {
    // ADR 0037: `background: true` ignores `timeout_secs`. The model
    // fills every optional field, so a small value it did not mean as a deadline must not kill
    // the job; the registry takes no deadline at all.
    let info = jobs.start(sandbox, cwd, &args.command)?;
    let cwd_relative = cwd
        .strip_prefix(root.root())
        .ok()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".into());
    let text = format!(
        "started background job {id}\n\
         its output is {output}\n\
         `timeout_secs` is ignored for background jobs; bound it in the command \
         (`timeout N …`) or stop it with `job` (`op: \"kill\"`).\n\
         read it with `job` (`op: \"read\"` reads the output so far, `op: \"wait\"` waits \
         for the end). `job` reads a bounded incremental ring; use `op: \"read\"` with \
         `tail_lines`, `pattern` or `from_line`/`max_lines` to see the whole file.\n\
         You will be notified when it finishes. Do not sleep or poll. If you have nothing \
         else to do, end your reply.\n",
        id = info.id,
        output = info.output,
    );
    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "background": true,
            "job_id": info.id,
            "output": info.output,
            "command": args.command,
            "cwd": cwd_relative,
            "sandbox_mode": sandbox.mode().as_str(),
            // ADR 0037: a background job has no deadline. Whether the model sent
            // the field is reported so a transcript shows it was dropped, not honoured.
            "timeout_secs": null,
            "timeout_secs_ignored": args.timeout_secs.is_some(),
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
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    sandbox: &Sandbox,
    cwd: &Path,
    command: &str,
    timeout: Duration,
    output_cap: usize,
    cancel: &CancellationToken,
    adopt: Option<Adopt<'_>>,
    retain: Option<Retention>,
) -> Result<ExecOutcome, ToolFailure> {
    let mut process = sandboxed_command(sandbox, cwd, command)?;

    let started = Instant::now();
    let mut supervised = Supervised::spawn(&mut process, output_cap, retain).map_err(|e| {
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
            Some(spec) => {
                let remaining = spec
                    .explicit_deadline
                    .map(|deadline| deadline.saturating_sub(started.elapsed()));
                match spec
                    .jobs
                    .adopt(&supervised, spec.command, started, remaining)
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
    /// The inline budget per stream (`output_cap`). The capture keeps the maximum of both
    /// views: the success head (`cap / 4`) and the failure tail (`cap - cap / 8`), so the
    /// buffer never exceeds `9/8` of the budget. The exit status is only known at the end,
    /// so both splits are trimmed at render time from this one capture.
    cap: usize,
    /// The largest head any view needs (the success head, `cap / 4`), kept verbatim.
    head_cap: usize,
    /// The largest tail any view needs (the failure tail, `cap - cap / 8`), kept as a ring.
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
    /// A capture with `cap` bytes of inline budget. The head keeps the success head
    /// (`cap / 4`) and the tail ring keeps the failure tail (`cap - cap / 8`); each view
    /// trims down to its own split at render time.
    fn with_cap(cap: usize) -> Self {
        let head_cap = cap / 4;
        Self {
            cap,
            head_cap,
            tail_cap: cap - cap / 8,
            ..Self::default()
        }
    }

    /// The inline split for this result: success keeps 1/4 head and 3/4 tail, failure keeps
    /// 1/8 head and 7/8 tail of the same budget. Failure is any non-zero exit, signal kill,
    /// timeout or cancellation: the error is at the end, so it gets the tail.
    fn split(&self, success: bool) -> (usize, usize) {
        if success {
            (self.cap / 4, self.cap - self.cap / 4)
        } else {
            (self.cap / 8, self.cap - self.cap / 8)
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

    /// Bytes the capture itself dropped (neither in `head` nor `tail`). A view can drop more
    /// by trimming to its split; see [`Capped::discarded_for`].
    fn capture_discarded(&self) -> u64 {
        self.total - (self.head.len() + self.tail.len()) as u64
    }

    /// The view for `success`: the first `head_need` bytes and the last `tail_need` bytes of
    /// the stream, plus how many bytes between them are hidden. Sliced from the one capture,
    /// so the buffer never grows past `9/8` of the budget while both splits stay available.
    fn view(&self, success: bool) -> (&[u8], &[u8], u64) {
        let (head_need, tail_need) = self.split(success);
        if self.capture_discarded() == 0 {
            // The capture holds the whole stream; show it whole when it fits the view.
            let full = self.total as usize;
            if full <= head_need + tail_need {
                return (&self.head, &self.tail, 0);
            }
            // Whole stream present but larger than the view: first `head_need`, last
            // `tail_need`. The halves concatenate to the stream, so index into them.
            let head_part = &self.head[..head_need.min(self.head.len())];
            let tail_from = full - tail_need;
            let tail_part = if tail_from >= self.head.len() {
                &self.tail[tail_from - self.head.len()..]
            } else {
                // The tail need reaches back into the head region; the head part already
                // covers it, so the view is head part plus the whole tail half.
                return (
                    head_part,
                    &self.tail,
                    full as u64 - head_part.len() as u64 - self.tail.len() as u64,
                );
            };
            let discarded = full as u64 - head_part.len() as u64 - tail_part.len() as u64;
            return (head_part, tail_part, discarded);
        }
        let head_part = &self.head[..head_need.min(self.head.len())];
        let tail_part = if self.tail.len() > tail_need {
            &self.tail[self.tail.len() - tail_need..]
        } else {
            &self.tail[..]
        };
        let discarded = self
            .total
            .saturating_sub((head_part.len() + tail_part.len()) as u64);
        (head_part, tail_part, discarded)
    }

    /// Bytes hidden from the `success` view (the number the marker names).
    fn discarded_for(&self, success: bool) -> u64 {
        self.view(success).2
    }

    fn is_truncated_for(&self, success: bool) -> bool {
        self.discarded_for(success) > 0
    }

    /// Bytes the `success` view shows (head plus tail, without the marker).
    fn shown_for(&self, success: bool) -> usize {
        let (head, tail, _) = self.view(success);
        head.len() + tail.len()
    }

    /// What the model sees for this result: the view's head, a marker naming the discarded
    /// bytes, then the view's tail. The failure view keeps less start and more end than the
    /// success view; a failing build's error message is almost always at the end.
    fn render_for(&self, success: bool) -> String {
        let (head, tail, discarded) = self.view(success);
        if discarded == 0 {
            let mut bytes = head.to_vec();
            bytes.extend_from_slice(tail);
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        let mut out = String::from_utf8_lossy(head).into_owned();
        out.push_str(&Self::marker_for(discarded));
        out.push_str(&String::from_utf8_lossy(tail));
        out
    }

    /// The marker that names the discarded middle, on its own line.
    fn marker_for(discarded: u64) -> String {
        format!("\n[... {discarded} bytes discarded; showing the start and the end ...]\n")
    }

    /// Writes the retained bytes (head, marker if the middle was dropped, then tail) to `out`.
    /// Used to seed a job's output file when a foreground command is adopted (ADR 0037
    /// revision 1). Returns the number of bytes written. The seed is the whole capture
    /// (up to `9/8` of the budget), not one view: the job file keeps the full history and
    /// the ring bounds the increment, so extra pre-adoption bytes are never lost here.
    fn write_to(&self, out: &mut impl Write) -> std::io::Result<u64> {
        out.write_all(&self.head)?;
        let mut written = self.head.len() as u64;
        let discarded = self.capture_discarded();
        if discarded > 0 {
            let marker = Self::marker_for(discarded);
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
    /// Tees every chunk to the session's retained-output file until the command is promoted to
    /// a job (ADR 0037); a truncated result is then recoverable through
    /// `shell_output`.
    retain: Option<Retention>,
}

struct JobSink {
    file: File,
    cell: Arc<Mutex<JobCell>>,
}

impl StreamSink {
    fn new(cap: usize, retain: Option<Retention>) -> Self {
        Self {
            capped: Capped::with_cap(cap),
            job: None,
            retain,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        match &mut self.job {
            None => {
                self.capped.push(bytes);
                if let Some(retain) = &self.retain {
                    retain.push(bytes);
                }
            }
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
        // Still running: no exit status yet, so the snapshot uses the success split (the same
        // proportions as before this change). The full capture seeds the job file.
        let rendered = self.capped.render_for(true);
        let written = self.capped.write_to(&mut file).unwrap_or(0);
        self.capped = Capped::default();
        // The job's own file is the retained output now; stop teeing to the foreground one.
        self.retain = None;
        self.job = Some(JobSink { file, cell });
        (written, rendered)
    }

    /// Takes the captured head+tail at end of stream. Empty for a stream that was promoted
    /// (its bytes went to the job instead). Trimmed to the capture caps, so memory stays
    /// bounded; each inline view trims further to its own split at render time.
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

impl Outcome {
    /// Success is exit code 0 without a timeout or cancellation. Anything else (non-zero
    /// exit, signal kill, timeout, cancellation) is a failure and gets the larger tail.
    fn succeeded(&self) -> bool {
        !self.timed_out && !self.cancelled && matches!(self.status.and_then(|s| s.code()), Some(0))
    }
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
    fn spawn(
        command: &mut Command,
        cap: usize,
        retain: Option<Retention>,
    ) -> std::io::Result<Self> {
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
        let stdout_sink = Arc::new(Mutex::new(StreamSink::new(cap, retain.clone())));
        let stderr_sink = Arc::new(Mutex::new(StreamSink::new(cap, retain)));
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

/// Reads to EOF, keeping the success head (`cap / 4`) and the failure tail (`cap - cap / 8`):
/// the maximum of both views, so the exit status — known only at the end — can still pick
/// either split at render time. Reading continues past the caps (and the tail is trimmed as
/// it goes), so a chatty command neither blocks on a full pipe nor grows this process's
/// memory with its output.
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

/// Puts `process` in its own session and process group (`setsid`), with no controlling
/// terminal. The same rule `shell` and background jobs use; public so ADR 0043's `subagent`
/// child (spawned by `radian-host`) cannot drift from it.
pub fn own_session(process: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` has no memory-safety preconditions and runs between fork and exec.
        unsafe {
            process.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process;
    }
}

#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// SIGKILLs every process in session `sid` ([`signal_session`]). The caller must hold the
/// leader **unreaped**, exactly as the job supervisor does, so a reused session id is never
/// hit. ADR 0043's subagent cancellation uses this; it is not a second kill path.
pub fn kill_session(sid: u32) {
    signal_session(sid, SIGKILL);
}

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;

/// Asks the **leader** `pid` — a session leader this process spawned — to stop gracefully:
/// SIGTERM to that pid alone, never to its whole session (ADR 0043 decision 1a, fix round 1 B).
///
/// The child handles it: `radian exec` cancels its turn and then shuts its session down, which
/// kills the whole session of every background job it started and stops its MCP servers
/// ([`kill_session`] is the fallback for a child that does not react). A session-wide SIGTERM
/// would instead hit processes the child is about to shut down in order.
pub fn stop_leader(pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            // SAFETY: `kill` takes plain integers and has no memory-safety preconditions.
            unsafe {
                libc::kill(pid, SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Blocks up to `grace` waiting for `pid` to exit, leaving it **unreaped** (`waitid` with
/// `WNOWAIT`), so its pid and session stay reserved for the SIGKILL fallback. `true` when it
/// exited.
///
/// The poll interval is not an ordering device: it only bounds how often the kernel is asked.
/// A drop path cannot await, so this is the synchronous counterpart of `child.wait()` with a
/// timeout; a caller that holds a `tokio::process::Child` should use that instead.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn wait_for_exit(pid: u32, grace: std::time::Duration) -> bool {
    const POLL: std::time::Duration = std::time::Duration::from_millis(20);
    let id: libc::id_t = pid;
    let deadline = std::time::Instant::now() + grace;
    loop {
        // SAFETY: `info` is zeroed plain data that `waitid` fills in; no other preconditions.
        let (status, exited) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let status = libc::waitid(
                libc::P_PID,
                id,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            (status, info.si_pid() == id as libc::pid_t)
        };
        if status == 0 && exited {
            return true;
        }
        if status != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
        {
            // Not a child of this process any more (`ECHILD`), or `waitid` refused: there is
            // nothing left to wait for, and `kill_session` on a gone pid would be unsafe.
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// Platforms without `waitid(WNOWAIT)` cannot tell a running child from a zombie one without
/// reaping it, so the caller goes straight to the SIGKILL fallback.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn wait_for_exit(_pid: u32, _grace: std::time::Duration) -> bool {
    false
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
pub fn signal_session(sid: u32, signal: i32) {
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
    outcome: &Outcome,
    retained: Option<RetainedView>,
) -> ToolOutput {
    let success = outcome.succeeded();
    let stdout = outcome.stdout.render_for(success);
    let stderr = outcome.stderr.render_for(success);
    let exit_code = outcome.status.and_then(|s| s.code());
    let signal = exit_code.is_none().then(|| status_signal(outcome.status));

    let mut text = String::new();
    text.push_str(&format!("$ {command}\n"));
    if outcome.timed_out {
        text.push_str(&format!(
            "[timed out after {timeout:?}; the whole session was killed. For a command that \
             needs longer, use `background: true` (no default timeout) and `job` with \
             `op: \"wait\"`.\n"
        ));
    }
    if outcome.cancelled {
        text.push_str("[cancelled; the whole session was killed]\n");
    }
    if outcome.slots_full {
        text.push_str(
            "[all background job slots were busy, so this call was **not** moved to the \
             background; it ran to its foreground timeout. Wait for a job with `job` \n             (`op: \"wait\"`), or stop one with `job` (`op: \"kill\"`).]\n",
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
    if outcome.stdout.is_truncated_for(success) {
        text.push_str(&format!(
            "[stdout truncated: {} of {} bytes discarded]\n",
            outcome.stdout.discarded_for(success),
            outcome.stdout.total
        ));
    }
    if outcome.stderr.is_truncated_for(success) {
        text.push_str(&format!(
            "[stderr truncated: {} of {} bytes discarded]\n",
            outcome.stderr.discarded_for(success),
            outcome.stderr.total
        ));
    }
    if let Some(retained) = &retained {
        let discarded =
            outcome.stdout.discarded_for(success) + outcome.stderr.discarded_for(success);
        text.push_str(&format!(
            "[{discarded} bytes were discarded from this view; the full output is retained. \
             Read it with `job read` job_id={}.]\n",
            retained.job_id
        ));
        if retained.cut {
            text.push_str(&format!(
                "[the retained output was cut to {} of {} bytes; {} bytes are not \
                 recoverable]\n",
                retained.retained_bytes,
                retained.total_bytes,
                retained.total_bytes.saturating_sub(retained.retained_bytes)
            ));
        }
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
            "stdout_bytes": outcome.stdout.shown_for(success),
            "stderr_bytes": outcome.stderr.shown_for(success),
            "stdout_total_bytes": outcome.stdout.total,
            "stderr_total_bytes": outcome.stderr.total,
            "stdout_truncated_bytes": outcome.stdout.discarded_for(success),
            "stderr_truncated_bytes": outcome.stderr.discarded_for(success),
            "job_id": retained.as_ref().map(|r| r.job_id.clone()),
            "output_retained_bytes": retained.as_ref().map(|r| r.retained_bytes),
            "output_total_bytes": retained.as_ref().map(|r| r.total_bytes),
            "output_retained_cut": retained.as_ref().map(|r| r.cut),
            "duration_ms": outcome.duration.as_millis() as u64,
        })),
    }
}

/// What a truncated foreground result exposes about its retained output: the job id the model
/// passes to `job`, and the retained/total byte counts so a cut is stated.
struct RetainedView {
    job_id: String,
    retained_bytes: u64,
    total_bytes: u64,
    cut: bool,
}

/// The result of a foreground call that was adopted by the job registry in place (ADR 0037
/// revision 1): the job id, the `job` hint, and a bounded tail of the output produced
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
    info: &crate::jobs::AdoptInfo,
) -> ToolOutput {
    let mut text = String::new();
    text.push_str(&format!("$ {command}\n"));
    text.push_str(&format!(
        "[still running after {threshold:?}; moved to the background as job {id} — the same \
         process, not restarted. Its output is {output}. Read it with `job` (`op: \"read\"`; \
         `op: \"wait\"` waits for the end). You will be notified when it finishes. Do not \
         sleep or poll. If you have nothing else to do, end your reply.]\n",
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
        // The capture keeps the maximum of both views: the success head and the failure
        // tail, so the buffer is `9/8` of the budget — a small constant factor.
        assert_eq!(capped.head.len(), cap / 4);
        assert_eq!(capped.tail.len(), cap - cap / 8);
        assert_eq!(
            capped.capture_discarded(),
            total as u64 - (cap / 4 + cap - cap / 8) as u64
        );
        // Either inline view is exactly the budget, never more.
        assert_eq!(capped.shown_for(true), cap);
        assert_eq!(capped.shown_for(false), cap);
        assert_eq!(capped.discarded_for(true), total as u64 - cap as u64);
        assert_eq!(capped.discarded_for(false), total as u64 - cap as u64);
    }

    #[test]
    fn the_shell_and_job_specs_fit_the_prefix_budget() {
        // ADR 0037 decision 4: the two definitions stay small enough for the cached prefix.
        // Compact `{"name","description","input_schema"}` JSON, UTF-8 bytes. All three
        // sandbox modes (the longest confinement text wins) and the one `job` tool.
        let size = |spec: &agent_core::ToolSpec| {
            serde_json::to_string(&serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "input_schema": spec.input_schema,
            }))
            .unwrap()
            .len()
        };
        for (mode, git_writable) in [
            (SandboxMode::FullAccess, false),
            (SandboxMode::WorkspaceWrite, true),
            (SandboxMode::WorkspaceWrite, false),
            (SandboxMode::Safe, false),
        ] {
            let bytes = size(&spec(mode, git_writable));
            assert!(
                bytes <= 2200,
                "shell ({mode:?}, git {git_writable}) is {bytes} B, over the 2,200 B cap"
            );
        }
        let bytes = size(&crate::jobs::job_spec());
        assert!(bytes <= 1800, "job is {bytes} B, over the 1,800 B cap");
    }

    #[test]
    fn a_short_stream_is_kept_whole() {
        let capped = read_capped(std::io::Cursor::new(b"hello".to_vec()), 64 << 10);
        assert_eq!(capped.total, 5);
        assert_eq!(capped.discarded_for(true), 0);
        assert_eq!(capped.discarded_for(false), 0);
        assert!(!capped.is_truncated_for(true));
        assert!(!capped.is_truncated_for(false));
        assert_eq!(capped.render_for(true), "hello");
        assert_eq!(capped.render_for(false), "hello");
    }

    #[test]
    fn the_last_bytes_survive_when_only_the_tail_fits() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        let capped = read_capped(std::io::Cursor::new(data.clone()), 40);

        // The capture keeps the success head (10) and the failure tail (35).
        assert_eq!(capped.head.len(), 10);
        assert_eq!(capped.tail.len(), 35);
        assert_eq!(capped.tail.as_slice(), &data[data.len() - 35..]);
        // Success shows 1/4 head and 3/4 tail of the same budget.
        let rendered = capped.render_for(true);
        assert!(rendered.contains("bytes discarded"), "{rendered}");
        let expected_tail = String::from_utf8_lossy(&data[data.len() - 30..]).into_owned();
        assert!(rendered.ends_with(&expected_tail), "{rendered}");
        assert_eq!(capped.shown_for(true), 40);
        // Failure shows less start (1/8) and more end (7/8): the very last byte survives.
        let failed = capped.render_for(false);
        assert!(failed.contains("bytes discarded"), "{failed}");
        let expected_failure_tail = String::from_utf8_lossy(&data[data.len() - 35..]).into_owned();
        assert!(failed.ends_with(&expected_failure_tail), "{failed}");
        assert_eq!(capped.shown_for(false), 40);
        let (success_head, _, _) = capped.view(true);
        let (failure_head, _, _) = capped.view(false);
        assert_eq!(success_head.len(), 10);
        assert_eq!(failure_head.len(), 5);
    }
}
