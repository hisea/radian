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
            review_pre_existing: None,
            review_settings: None,
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

struct OneCommand {
    exit_code: Option<i32>,
    duration_ms: u64,
    outcome: SeriesOutcome,
}

fn run_one(
    checkout: &Path,
    line: &str,
    timeout: Duration,
    log: &Path,
) -> Result<OneCommand, GateError> {
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
        .current_dir(checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(stderr));
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
    let deadline = (!timeout.is_zero()).then(|| Instant::now() + timeout);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait().map_err(|source| GateError::Io {
            path: checkout.to_path_buf(),
            source,
        })? {
            break (status, false);
        }
        #[cfg(unix)]
        running.stopped();
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            #[cfg(unix)]
            kill_group(pid);
            #[cfg(not(unix))]
            let _ = child.kill();
            let status = child.wait().map_err(|source| GateError::Io {
                path: checkout.to_path_buf(),
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
    Ok(OneCommand {
        exit_code: status.code(),
        duration_ms,
        outcome,
    })
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
