//! The `commands` stage (ADR 0048 decision 10).
//!
//! The profile's fixed command lines run in order, with `sh -c`, in the stage's own
//! checkout of the verification candidate. The stage stops at the first failure. Each command
//! has its own log file and its own timeout; a timeout kills the command's whole process
//! group.

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::process;

use super::checkout::GateCheckout;
use super::config::Stage;
use super::error::GateError;
use super::exclusive::ExclusiveLock;
use super::integration::candidate_for;
use super::verdict::{
    CommandRecord, VERDICT_SCHEMA_VERSION, Verdict, VerdictResult, attempt_dir, now_utc,
    verdict_path,
};

/// The `commands` stage for one attempt and one sha.
///
/// The stage holds the exclusive lock for its whole run, builds the verification candidate
/// in `<repository>/.radian/arc/gates/commands/` (fetching from `remote`), runs the lines on
/// it and writes the verdict. `timeout` of zero waits forever for a command.
#[derive(Debug, Clone)]
pub struct CommandRun<'a> {
    /// The repository root.
    pub repository: &'a Path,
    /// The repository's remote name, for the fetches before the checkout.
    pub remote: &'a str,
    /// The task id (one path component).
    pub task: &'a str,
    /// The attempt id (one path component).
    pub attempt: &'a str,
    /// The task's `verified_sha`; the verdict's `sha` and the commit the candidate contains.
    pub sha: &'a str,
    /// The ref that names the remote main after a fetch, typically
    /// `refs/remotes/<remote>/<main>`.
    pub main_ref: &'a str,
    /// The profile's acceptance command lines.
    pub commands: &'a [String],
    /// The gate profile the stage runs under, for the verdict (ADR 0048 decisions 10 and 15).
    pub profile: &'a str,
    /// Per-command timeout.
    pub timeout: Duration,
}

impl CommandRun<'_> {
    /// Runs the stage and returns the verdict it wrote.
    ///
    /// # Errors
    ///
    /// The exclusive lock, the checkout, a command's log file, or the verdict write failed.
    /// A failing command is not an error: it is a `fail` verdict.
    pub fn run(&self) -> Result<Verdict, GateError> {
        let _lock = ExclusiveLock::acquire(self.repository)?;
        let started_at = now_utc();
        let checkout = GateCheckout::new(self.repository, Stage::Commands, self.remote);
        let candidate = candidate_for(
            self.repository,
            self.remote,
            self.main_ref,
            self.sha,
            self.task,
            &checkout,
        )?;
        let log_dir = attempt_dir(self.repository, self.task, self.attempt)?.join("logs");
        // A candidate that could not be built is a failed stage; the commands never run on a
        // tree without a candidate (ADR 0048 decision 10).
        let (commands, outcome) = if candidate.failure.is_some() {
            (Vec::new(), SeriesOutcome::Failed)
        } else {
            run_series(
                &checkout.path(),
                self.commands,
                self.timeout,
                &log_dir,
                Stage::Commands,
            )?
        };
        let ended_at = now_utc();
        let verdict = Verdict {
            schema_version: VERDICT_SCHEMA_VERSION,
            stage: Stage::Commands,
            sha: candidate.sha,
            result: outcome.result(),
            reason: candidate.failure.or_else(|| outcome.reason()),
            commands,
            started_at,
            ended_at,
            main_sha: Some(candidate.main_sha),
            base_sha: None,
            candidate_sha: candidate.candidate_sha,
            conflict_paths: candidate.conflict_paths,
            approver: None,
            review: None,
            review_session_ids: Vec::new(),
            review_cost_micros: None,
            review_model_error: None,
            review_pre_existing: None,
            review_settings: None,
            profile: Some(self.profile.to_owned()),
            integration_style: None,
        };
        verdict.write_new(&verdict_path(
            self.repository,
            self.task,
            self.attempt,
            Stage::Commands,
        )?)?;
        Ok(verdict)
    }
}

/// How a command series ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeriesOutcome {
    /// Every command exited zero.
    Passed,
    /// A command exited non-zero.
    Failed,
    /// A command ran past its timeout and was killed.
    TimedOut,
}

impl SeriesOutcome {
    pub(crate) fn result(self) -> VerdictResult {
        match self {
            SeriesOutcome::Passed => VerdictResult::Pass,
            SeriesOutcome::Failed => VerdictResult::Fail,
            SeriesOutcome::TimedOut => VerdictResult::Fail,
        }
    }

    pub(crate) fn reason(self) -> Option<String> {
        match self {
            SeriesOutcome::Passed => None,
            SeriesOutcome::Failed => Some("command_failed".to_string()),
            SeriesOutcome::TimedOut => Some("timeout".to_string()),
        }
    }
}

/// Runs command lines in order in `checkout`, stopping at the first failure.
///
/// Shared by the `commands` stage and the integration stage's key commands, which follow
/// the same rules. Logs are `<log_dir>/<stage>-<n>.log`, one per executed command.
pub(crate) fn run_series(
    checkout: &Path,
    lines: &[String],
    timeout: Duration,
    log_dir: &Path,
    stage: Stage,
) -> Result<(Vec<CommandRecord>, SeriesOutcome), GateError> {
    std::fs::create_dir_all(log_dir).map_err(|source| GateError::Io {
        path: log_dir.to_path_buf(),
        source,
    })?;
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let log = log_dir.join(format!("{}-{:02}.log", stage.as_str(), index + 1));
        let one = run_one(checkout, line, timeout, &log)?;
        records.push(CommandRecord {
            line: line.clone(),
            exit_code: one.exit_code,
            duration_ms: one.duration_ms,
            log: log.to_string_lossy().into_owned(),
        });
        match one.outcome {
            SeriesOutcome::Passed => {}
            other => return Ok((records, other)),
        }
    }
    Ok((records, SeriesOutcome::Passed))
}

/// What one shell command did: the `commands`/integration line, or a wrapper
/// `before_attempt` command (ADR 0048 decisions 7 and 10). The caller decides what a non-zero
/// exit or a timeout means for its step.
pub(crate) struct ShellOutcome {
    /// The process's exit code; `None` when a signal killed it.
    pub(crate) exit_code: Option<i32>,
    /// How long it ran, in milliseconds.
    pub(crate) duration_ms: u64,
    /// Passed, failed, or was killed at the timeout.
    pub(crate) outcome: SeriesOutcome,
}

impl ShellOutcome {
    pub(crate) fn passed(&self) -> bool {
        matches!(self.outcome, SeriesOutcome::Passed)
    }

    pub(crate) fn timed_out(&self) -> bool {
        matches!(self.outcome, SeriesOutcome::TimedOut)
    }
}

fn run_one(
    checkout: &Path,
    line: &str,
    timeout: Duration,
    log: &Path,
) -> Result<ShellOutcome, GateError> {
    run_shell(checkout, line, timeout, log, &[], &[], None)
}

/// What can stop a [`run_shell`] command from outside the process that started it (ADR 0048
/// decisions 7, 9 and 10). The gate stages reach their child through [`super::stage::Running`];
/// the wrapper's `before_attempt` command implements this so its own SIGTERM handler and
/// `radian arc cancel` reach the command's process group the way they reach a worker's.
pub(crate) trait ShellWatch {
    /// The command now runs in its own process group `pgid`; a stop must reach that group.
    fn started(&self, pgid: u32);
    /// The command was reaped; the group id must not be signalled after this.
    fn finished(&self);
    /// Whether a stop was requested while the command ran.
    fn stopping(&self) -> bool;
    /// How long the group gets after SIGTERM before SIGKILL.
    fn grace(&self) -> Duration;
}

/// Runs one `sh -c` line in `dir`, stdout and stderr into `log`, in its own process group.
/// `timeout` of zero waits forever; a timeout kills the whole group, so no child the command
/// started outlives the step. A stop from `watch` also reaches the whole group: the command's
/// own shell exiting on SIGTERM does not end the stop while one of its children still runs.
/// `envs` are added on top of the wrapper's own environment; the gate stages pass none, the
/// wrapper's `before_attempt` commands pass the `ARC_*` values a command worker gets (ADR
/// 0048 decisions 7 and 10). `remove` names variables the command must not inherit: the
/// `[integration] command` passes [`crate::git::REPOSITORY_ENV`], so a caller's environment
/// cannot point the command's own git calls at another repository (ADR 0048 decision 6).
/// `watch` is the caller's stop path: it receives the command's process group and is asked
/// whether a stop was requested.
pub(crate) fn run_shell(
    dir: &Path,
    line: &str,
    timeout: Duration,
    log: &Path,
    envs: &[(&str, String)],
    remove: &[&str],
    watch: Option<&dyn ShellWatch>,
) -> Result<ShellOutcome, GateError> {
    let file = File::create(log).map_err(|source| GateError::Io {
        path: log.to_path_buf(),
        source,
    })?;
    let stderr = file.try_clone().map_err(|source| GateError::Io {
        path: log.to_path_buf(),
        source,
    })?;
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr));
    for (name, value) in envs {
        command.env(name, value);
    }
    for name in remove {
        command.env_remove(name);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // The command gets its own process group, so a timeout reaches every child it
        // started and not just `sh` itself.
        command.process_group(0);
    }

    let started = Instant::now();
    let mut child = command.spawn().map_err(|source| GateError::Spawn {
        program: "sh".to_string(),
        source,
    })?;
    #[cfg(unix)]
    let pid = child.id();
    // A stop from `serve` reaches this child's group, so the next loop never finds a
    // build writing in the checkout the gate left (ADR 0048 decision 10).
    #[cfg(unix)]
    let mut running = super::stage::Running::track(pid);
    // The wrapper's `before_attempt` command is stopped through the wrapper's own handler and
    // signal file; its group is registered there for exactly as long as the command runs.
    #[cfg(unix)]
    let _watched = watch.map(|watch| {
        watch.started(pid);
        Watched { watch }
    });
    #[cfg(unix)]
    let mut stop_sent: Option<Instant> = None;
    let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
    let (status, timed_out) = loop {
        // Peek at the exit without reaping it where the platform can: while the child is a
        // zombie its process-group id stays reserved, so a stop can still reach whatever the
        // command left in its group (ADR 0048 decisions 7 and 9).
        #[cfg(unix)]
        let exited = process::exited_status(pid).map_err(|source| GateError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        #[cfg(not(unix))]
        let exited = child.try_wait().map_err(|source| GateError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        if exited.is_some() {
            // The shell of a stopped command may exit on SIGTERM while a child it started
            // ignores it, and it can exit before this loop observes the stop. Nothing the
            // command started outlives the stop: kill the rest of the group before the zombie
            // is reaped and its group id can name another.
            #[cfg(unix)]
            if watch.is_some_and(|watch| watch.stopping()) && process::group_has_live_member(pid) {
                process::signal_group(pid, process::SIGKILL);
            }
            let status = child.wait().map_err(|source| GateError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            break (status, false);
        }
        #[cfg(unix)]
        running.stopped();
        #[cfg(unix)]
        if let Some(watch) = watch
            && watch.stopping()
        {
            let now = Instant::now();
            match stop_sent {
                None => {
                    process::signal_group(pid, process::SIGTERM);
                    stop_sent = Some(now);
                }
                Some(at) if now >= at + watch.grace() => {
                    process::signal_group(pid, process::SIGKILL);
                }
                Some(_) => {}
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            #[cfg(unix)]
            kill_group(pid);
            #[cfg(not(unix))]
            let _ = child.kill();
            let status = child.wait().map_err(|source| GateError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            break (status, true);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // A stop killed the child: this is not a result of the stage, and no verdict may be
    // written for it (ADR 0048 decision 13).
    if super::stage::stop_requested() {
        return Err(GateError::Stopped);
    }
    let outcome = if timed_out {
        SeriesOutcome::TimedOut
    } else if status.success() {
        SeriesOutcome::Passed
    } else {
        SeriesOutcome::Failed
    };
    Ok(ShellOutcome {
        exit_code: status.code(),
        duration_ms,
        outcome,
    })
}

#[cfg(unix)]
struct Watched<'a> {
    watch: &'a dyn ShellWatch,
}

#[cfg(unix)]
impl Drop for Watched<'_> {
    fn drop(&mut self) {
        // Runs on every return path, so the wrapper's stop handler can never signal a group
        // whose command has already been reaped.
        self.watch.finished();
    }
}

#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    // SAFETY: `kill` is a syscall with no memory operands; a negative pid signals the
    // process group whose id is `pid`, which `process_group(0)` made the child lead.
    // A group that already exited yields ESRCH, which is ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
