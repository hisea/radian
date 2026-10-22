//! Running one attempt and stopping it (ADR 0048 decisions 6, 7, 9 and 12).
//!
//! The owner drives one task at a time in v0: `radian arc start <task>` claims the task, runs
//! the trusted wrapper as a child (`radian arc work`, the current executable), watches the
//! attempt's jsonl stream for a stall and its clock for `max_wall_time_secs`, and applies the
//! wrapper's result in one transaction. `radian arc cancel <task>` stops a running attempt
//! the same way: SIGTERM to the wrapper, a grace period, then SIGKILL.
//!
//! There is no in-process scheduling loop here: [`crate::run`] owns the loop and calls
//! [`run_claim`] for every attempt it claims. [`reconcile`] is the restart step the loop and
//! `start` and `cancel` call (decision 9, last bullet).

use std::fs::{self, File};
use std::io::Write;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use super::process;
use crate::store::attempt_ops::{self, Applied, Claim, Class};
use crate::store::{Error, Store, model};

pub use crate::store::attempt_ops::FAILURE_CLASSES;

/// How often the monitor looks at the child and its jsonl stream.
const POLL: Duration = Duration::from_millis(100);

/// What one `start` did: the reconcile results it applied first, then its own attempt, then
/// the verification of a pushed attempt when the task reached `review.verifying`.
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub reconciled: Vec<Applied>,
    /// The attempt this call ran. `None` when the task was already `review.verifying` and
    /// the call only resumed verification.
    pub applied: Option<Applied>,
    /// The verification that followed the attempt. `None` when the task did not reach
    /// `review.verifying`; the inner error is a refusal that left the task there (a moved
    /// branch, a stage this build does not run yet).
    pub verification: Option<Result<crate::review::Verified, Error>>,
}

/// What one `cancel` did.
#[derive(Debug, Clone, PartialEq)]
pub struct Canceled {
    pub applied: Option<Applied>,
    pub state: String,
    pub phase: String,
}

impl Canceled {
    /// The one line `radian arc cancel` prints.
    pub fn line(&self, task: &str) -> String {
        format!("{task} {}.{}", self.state, self.phase)
    }
}

/// `radian arc start <task>`: reconcile, release due retries, claim, run one attempt, apply.
///
/// A task already in `review.verifying` (one a moved branch or a changed digest sent back)
/// resumes verification instead of being refused.
pub fn start(store: &Store, task_id: &str) -> Result<Started, Error> {
    // The worker's command line is checked before anything is claimed or written: a wrong
    // `[worker] exec_args` entry would otherwise fail every attempt the same way, after the
    // claim (ADR 0048 decision 7).
    crate::worker_args::check(&store.config()?.worker.exec_args)?;
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    if task.state == model::TaskState::Review && task.phase == "verifying" {
        return Ok(Started {
            reconciled: Vec::new(),
            applied: None,
            verification: Some(crate::review::verify(store, task_id)),
        });
    }
    let reconciled = reconcile(store)?;
    // The loop releases a due retry on every pass; `start` is the owner's one task, so it
    // does the same before it claims.
    attempt_ops::release_retries(store, &crate::store::plan_ops::utc_now())?;
    let config = store.config()?;
    let claim = attempt_ops::claim(store, task_id)?;
    let task = store.load_task(task_id)?.item;
    let applied = run_claim(store, &claim, &task, &config)?;
    // A pushed attempt continues into the gate in the same process (ADR 0048 decision 6):
    // one `start` takes the task from `todo.ready` to `review.awaiting_human` or
    // `review.rework_pending`. A refused verification leaves the task in `review.verifying`
    // and is reported after the attempt's own line.
    let verification = (applied.ignored.is_none()
        && applied.state == model::TaskState::Review.as_str()
        && applied.phase == "verifying")
        .then(|| crate::review::verify(store, task_id));
    Ok(Started {
        reconciled,
        applied: Some(applied),
        verification,
    })
}

/// Runs one claimed attempt: the wrapper child, its monitor, and the result transaction.
/// The scheduling loop claims several tasks and calls this for each, from one thread per
/// attempt when `[limits] concurrency` is greater than one.
pub fn run_claim(
    store: &Store,
    claim: &Claim,
    task: &model::TaskFile,
    config: &crate::store::config::Config,
) -> Result<Applied, Error> {
    let task_id = claim.task.as_str();
    let (remote, main_branch) = store.repository_names()?;
    let run_dir = attempt_ops::run_dir(store, task_id, claim.attempt);
    fs::create_dir_all(&run_dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", run_dir.display())))?;
    let stdout_path = run_dir.join("wrapper.stdout.log");
    let stderr_path = run_dir.join("wrapper.stderr.log");
    let started_at = Instant::now();

    let executable = std::env::current_exe()
        .map_err(|error| Error::Failed(format!("cannot find the current executable: {error}")))?;
    let mut command = Command::new(executable);
    command
        .arg("arc")
        .arg("work")
        .arg("--task")
        .arg(task_id)
        .arg("--attempt")
        .arg(claim.attempt.to_string())
        .arg("--token")
        .arg(&claim.token)
        .arg("--remote")
        .arg(&remote)
        .arg("--main-branch")
        .arg(&main_branch);
    if claim.resume {
        command.arg("--resume");
    }
    if claim.allow_noop {
        command.arg("--allow-noop");
    }
    command.arg("--");
    command.args(&config.worker.exec_args);
    let stdout = File::create(&stdout_path).map_err(|error| {
        Error::Failed(format!("cannot create {}: {error}", stdout_path.display()))
    })?;
    let stderr = File::create(&stderr_path).map_err(|error| {
        Error::Failed(format!("cannot create {}: {error}", stderr_path.display()))
    })?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|error| Error::Failed(format!("cannot start `radian arc work`: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(claim.brief.as_bytes());
    }
    // Move the task to `doing.running` once the wrapper started. A cancel may have moved it
    // already: the closure restarts `running` only over the same attempt's `starting`.
    if let Err(error) = store.change_task(task_id, |task| {
        if task.state == model::TaskState::Doing
            && task.phase == "starting"
            && task.attempt == claim.attempt
        {
            task.phase = "running".to_owned();
        }
        Ok(())
    }) {
        // The task file changed under the new attempt: stop the wrapper rather than leave an
        // attempt running that arc may never apply.
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }

    let limits = Limits::of(task, config);
    let status = monitor(
        store,
        task_id,
        claim.attempt,
        &run_dir,
        &mut child,
        &limits,
        started_at,
    )?;
    let signal = attempt_ops::read_signal(store, task_id, claim.attempt);
    let finish = attempt_ops::read_finish(store, task_id, claim.attempt);
    attempt_ops::apply(
        store,
        task_id,
        claim.attempt,
        finish.as_ref(),
        signal,
        status.code(),
    )
}

/// One attempt's time limits.
struct Limits {
    stall: Duration,
    wall: Duration,
    grace: Duration,
}

impl Limits {
    fn of(task: &model::TaskFile, config: &crate::store::config::Config) -> Self {
        let seconds = |task: Option<u64>, config: u64| Duration::from_secs(task.unwrap_or(config));
        Self {
            stall: seconds(task.stall_timeout_secs, config.limits.stall_timeout_secs),
            wall: seconds(task.max_wall_time_secs, config.limits.max_wall_time_secs),
            grace: Duration::from_secs(config.limits.cancel_grace_secs),
        }
    }
}

/// Waits for the wrapper, and stops it with SIGTERM (then SIGKILL after the grace period)
/// when the attempt stalls, runs past its wall time, or a cancel recorded a signal.
fn monitor(
    store: &Store,
    task_id: &str,
    attempt: u32,
    run_dir: &std::path::Path,
    child: &mut Child,
    limits: &Limits,
    started_at: Instant,
) -> Result<ExitStatus, Error> {
    let stream = run_dir.join("result.jsonl");
    let mut last_len = file_len(&stream);
    let mut last_growth = Instant::now();
    let mut signalled: Option<Class> = None;
    let mut kill_at: Option<Instant> = None;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| Error::Failed(format!("cannot wait for the wrapper: {error}")))?
        {
            return Ok(status);
        }
        let now = Instant::now();
        let len = file_len(&stream);
        if len != last_len {
            last_len = len;
            last_growth = now;
        }
        if signalled.is_none() {
            // A cancel from another process writes its own signal; the monitor does not add a
            // second reason.
            let reason = if let Some(reason) = attempt_ops::read_signal(store, task_id, attempt)
                && reason == Class::Cancel
            {
                Some(Class::Cancel)
            } else if !limits.wall.is_zero() && now.duration_since(started_at) >= limits.wall {
                Some(Class::WallTime)
            } else if !limits.stall.is_zero() && now.duration_since(last_growth) >= limits.stall {
                Some(Class::Stall)
            } else {
                None
            };
            if let Some(reason) = reason {
                attempt_ops::write_signal(store, task_id, attempt, reason)?;
                process::signal(child.id(), process::SIGTERM);
                signalled = Some(reason);
                kill_at = Some(now + limits.grace);
            }
        }
        if let Some(at) = kill_at
            && now >= at
        {
            process::signal(child.id(), process::SIGKILL);
            kill_at = None;
        }
        std::thread::sleep(POLL);
    }
}

fn file_len(path: &std::path::Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// The reconcile step (decision 9, last bullet): for every task in `doing`, apply a result
/// that is there, leave a live attempt alone, and turn a dead attempt without a result into
/// an `infrastructure` (terminated) retry. It never starts a task, and it leaves
/// `doing.reconciling` for the owner.
pub fn reconcile(store: &Store) -> Result<Vec<Applied>, Error> {
    let mut applied = Vec::new();
    for id in store.task_ids()? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc {
            continue;
        }
        let task = loaded.item;
        if task.state != model::TaskState::Doing || task.phase == "reconciling" {
            continue;
        }
        let attempt = task.attempt;
        if attempt == 0 {
            continue;
        }
        let signal = attempt_ops::read_signal(store, &id, attempt);
        if let Some(finish) = attempt_ops::read_finish(store, &id, attempt) {
            let result = attempt_ops::apply(store, &id, attempt, Some(&finish), signal, None)?;
            if result.ignored.is_none() {
                applied.push(result);
                continue;
            }
            // An old attempt's record is not this attempt's result: the process check below
            // decides.
        }
        if wrapper_process(store, &id, attempt)
            .is_some_and(|(pid, start)| process::same_process(pid, &start))
        {
            continue;
        }
        if start_process(store, &id, attempt)
            .is_some_and(|(pid, start)| process::same_process(pid, &start))
        {
            continue;
        }
        let result = attempt_ops::apply(store, &id, attempt, None, signal, None)?;
        if result.ignored.is_none() {
            applied.push(result);
        }
    }
    Ok(applied)
}

/// The wrapper's `(pid, start time)` from the attempt record.
fn wrapper_process(store: &Store, task: &str, attempt: u32) -> Option<(u32, String)> {
    let record = read_json(&attempt_ops::attempt_dir(store, task, attempt).join("identity.json"))?;
    Some((
        record.get("wrapper_pid")?.as_u64()? as u32,
        record
            .get("wrapper_process_started_at")?
            .as_str()?
            .to_owned(),
    ))
}

/// The `start` process's `(pid, start time)` from the claim's record.
fn start_process(store: &Store, task: &str, attempt: u32) -> Option<(u32, String)> {
    let record = read_json(&attempt_ops::attempt_dir(store, task, attempt).join("start.json"))?;
    Some((
        record.get("pid")?.as_u64()? as u32,
        record.get("process_started_at")?.as_str()?.to_owned(),
    ))
}

fn read_json(path: &std::path::Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `radian arc cancel <task>`: stop one task.
///
/// A `todo` task goes to `failed.cancelled`. A `doing` task gets SIGTERM on its wrapper (the
/// pid and its start time must match, so a reused pid is never signalled), then SIGKILL after
/// `[limits] cancel_grace_secs`. When the wrapper's `AttemptFinished` arrives the task goes to
/// `failed.cancelled`; when it never arrives the task goes to `doing.reconciling`.
pub fn cancel(store: &Store, task_id: &str) -> Result<Canceled, Error> {
    let _ = reconcile(store)?;
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    match task.state {
        model::TaskState::Todo => {
            store.change_task(task_id, |task| {
                task.state = model::TaskState::Failed;
                task.phase = "cancelled".to_owned();
                task.retry_at = None;
                Ok(())
            })?;
            Ok(Canceled {
                applied: None,
                state: "failed".to_owned(),
                phase: "cancelled".to_owned(),
            })
        }
        model::TaskState::Doing if task.phase == "reconciling" => {
            // The owner resolves a reconciling task with the same process check: only the
            // process the record names is stopped.
            if let Some((pid, start)) = wrapper_process(store, task_id, task.attempt)
                && process::same_process(pid, &start)
            {
                let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
                process::signal(pid, process::SIGTERM);
                let deadline = Instant::now() + grace;
                while Instant::now() < deadline && process::same_process(pid, &start) {
                    std::thread::sleep(POLL);
                }
                if process::same_process(pid, &start) {
                    process::signal(pid, process::SIGKILL);
                }
            }
            store.change_task(task_id, |task| {
                if task.state == model::TaskState::Doing && task.phase == "reconciling" {
                    task.state = model::TaskState::Failed;
                    task.phase = "cancelled".to_owned();
                }
                Ok(())
            })?;
            let after = store.load_task(task_id)?.item;
            Ok(Canceled {
                applied: None,
                state: after.state.as_str().to_owned(),
                phase: after.phase,
            })
        }
        model::TaskState::Doing => cancel_running(store, task_id, &task),
        model::TaskState::Review | model::TaskState::Done | model::TaskState::Failed => {
            Err(Error::Refused(format!(
                "task `{task_id}` is `{}.{}`; only a `todo` or `doing` task can be cancelled",
                task.state.as_str(),
                task.phase
            )))
        }
    }
}

fn cancel_running(store: &Store, task_id: &str, task: &model::TaskFile) -> Result<Canceled, Error> {
    let attempt = task.attempt;
    attempt_ops::write_signal(store, task_id, attempt, Class::Cancel)?;
    let config = store.config()?;
    let grace = Duration::from_secs(config.limits.cancel_grace_secs);
    if let Some((pid, start)) = wrapper_process(store, task_id, attempt)
        && process::same_process(pid, &start)
    {
        process::signal(pid, process::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline
            && attempt_ops::read_finish(store, task_id, attempt).is_none()
        {
            std::thread::sleep(POLL);
        }
        if attempt_ops::read_finish(store, task_id, attempt).is_none() {
            // The grace period ran out: SIGKILL, but only the process that still has the
            // recorded start time.
            if process::same_process(pid, &start) {
                process::signal(pid, process::SIGKILL);
            }
        }
    }
    if let Some(finish) = attempt_ops::read_finish(store, task_id, attempt) {
        let applied = attempt_ops::apply(
            store,
            task_id,
            attempt,
            Some(&finish),
            Some(Class::Cancel),
            None,
        )?;
        if applied.ignored.is_none() {
            return Ok(Canceled {
                applied: Some(applied),
                state: "failed".to_owned(),
                phase: "cancelled".to_owned(),
            });
        }
    }
    // The wrapper never confirmed its end. `doing.reconciling` is the owner's decision; a
    // cancel from another process may have applied the result first, so the task is re-read.
    store.change_task(task_id, |task| {
        if task.state == model::TaskState::Doing
            && task.phase != "reconciling"
            && task.attempt == attempt
        {
            task.phase = "reconciling".to_owned();
        }
        Ok(())
    })?;
    let after = store.load_task(task_id)?.item;
    Ok(Canceled {
        applied: None,
        state: after.state.as_str().to_owned(),
        phase: after.phase,
    })
}

/// The lines `radian arc start` prints: any reconcile lines, then its own attempt's line, then
/// the verification's when the task reached `review.verifying`.
pub fn lines(started: &Started) -> Vec<String> {
    let mut lines: Vec<String> = started
        .reconciled
        .iter()
        .map(|applied| format!("reconciled: {}", applied.line()))
        .collect();
    if let Some(applied) = &started.applied {
        lines.push(applied.line());
    }
    if let Some(Ok(verified)) = &started.verification {
        lines.push(verified.line());
    }
    lines
}
