//! Running one attempt and stopping it (ADR 0048 decisions 6, 7, 9 and 12).
//!
//! [`spawn_claim`] starts the trusted wrapper as a child (`radian arc work`, the current
//! executable), hands it the brief and returns; the wrapper owns the attempt's limits (stall,
//! wall time, cancel) from then on. [`run_claim`] waits for it and applies the result in one
//! transaction, which is what `radian arc start` does; a scheduling pass uses [`spawn_claim`]
//! and leaves the result to its next [`reconcile`].
//!
//! `radian arc cancel <id>` stops one task, or a whole plan: SIGTERM to the wrapper, a grace
//! period, then SIGKILL. [`reconcile`] is the restart step the loop and `start` and `cancel`
//! call (decision 9, last bullet); it decides liveness from the wrapper's heartbeat and never
//! from `ps`, which the `workspace` sandbox refuses.

use std::fs::{self, File};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::process;
use crate::store::attempt_ops::{self, Applied, Claim, Class, WorkerLine};
use crate::store::{Error, Store, model};

pub use crate::store::attempt_ops::FAILURE_CLASSES;

/// How often the monitor looks at the child and its jsonl stream.
const POLL: Duration = Duration::from_millis(100);

/// How long `cancel` keeps waiting for the wrapper's `AttemptFinished` after the grace period.
///
/// The grace itself belongs to the process the wrapper is stopping: the worker's group gets
/// SIGTERM for `[limits] cancel_grace_secs` and then SIGKILL, and a `before_attempt` command's
/// group gets the same. The wrapper observes the stop a poll or so after `cancel` signals it and
/// still has to reap what it killed and write its record, so `cancel` reserves this window for
/// that work instead of shortening the grace to make room for it. Only after the grace and this
/// window has passed without a record does `cancel` SIGKILL a wrapper that is still running.
pub const CANCEL_RECORD_RESERVE: Duration = Duration::from_secs(1);

/// What one `start` did: the reconcile results it applied first, then its own attempt, then
/// the verification of a pushed attempt when the task reached `review.verifying`.
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub reconciled: Vec<Applied>,
    /// The warning of a command worker with a cost limit, printed once per attempt before the
    /// attempt's own line (ADR 0048 decisions 7 and 12).
    pub no_cost_warning: Option<String>,
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
    /// The task the line names.
    pub task: String,
    pub applied: Option<Applied>,
    pub state: String,
    pub phase: String,
}

impl Canceled {
    /// The one line `radian arc cancel` prints for this task.
    pub fn line(&self) -> String {
        format!("{} {}.{}", self.task, self.state, self.phase)
    }
}

/// What a plan's cancel did: every task it changed, in the plan's task order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCanceled {
    pub plan: String,
    pub tasks: Vec<Canceled>,
}

impl PlanCanceled {
    /// The lines `radian arc cancel <plan>` prints: one per changed task, then the plan.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.tasks.iter().map(Canceled::line).collect();
        lines.push(format!("{} cancelled", self.plan));
        lines
    }
}

/// What `radian arc cancel <id>` did: the task form or the plan form.
#[derive(Debug, Clone, PartialEq)]
pub enum Cancelled {
    Task(Box<Canceled>),
    Plan(PlanCanceled),
}

impl Cancelled {
    /// The lines the command prints, one per line.
    pub fn lines(&self) -> Vec<String> {
        match self {
            Cancelled::Task(canceled) => vec![canceled.line()],
            Cancelled::Plan(canceled) => canceled.lines(),
        }
    }
}

/// `radian arc start <task>`: reconcile, release due retries, claim, run one attempt, apply.
///
/// A task already in `review.verifying` (one a moved branch or a changed digest sent back)
/// resumes verification instead of being refused.
pub fn start(store: &Store, task_id: &str) -> Result<Started, Error> {
    // The worker's line is checked before anything is claimed or written: a wrong
    // `[worker] exec_args` entry, or a `[worker] command` program that does not resolve,
    // would otherwise fail every attempt the same way, after the claim (ADR 0048 decision 7).
    crate::worker_args::check_config(&store.config()?, store.repo())?;
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    // A cancelled plan is final: none of its tasks starts or resumes verification (ADR 0048
    // decisions 3 and 9). The plan's tasks decide too, so a plan whose tasks are all
    // `failed.cancelled` refuses its tasks the same way.
    store.refuse_cancelled(&task.plan)?;
    if task.state == model::TaskState::Review && task.phase == "verifying" {
        return Ok(Started {
            reconciled: Vec::new(),
            no_cost_warning: None,
            applied: None,
            verification: Some(crate::review::verify(store, task_id)),
        });
    }
    let reconciled = reconcile(store)?;
    // The loop releases a due retry on every pass; `start` is the owner's one task, so it
    // does the same before it claims. A target whose `retry_at` is still in the future is
    // claimed anyway: the claim accepts `todo.retry_wait`, and the backoff exists for the
    // scheduling loop (ADR 0048 decision 9).
    attempt_ops::release_retries(store, &crate::store::plan_ops::utc_now())?;
    let config = store.config()?;
    let claim = attempt_ops::claim(store, task_id)?;
    let no_cost_warning = claim.no_cost_warning.clone();
    let task = store.load_task(task_id)?.item;
    let applied = run_claim(store, &claim, &task, &config)?;
    // A pushed attempt continues into the gate in the same process (ADR 0048 decision 6):
    // one `start` takes the task from `todo.ready` or `todo.retry_wait` to
    // `review.awaiting_human` or `review.rework_pending`. A refused verification leaves the
    // task in `review.verifying` and is reported after the attempt's own line.
    let verification = (applied.ignored.is_none()
        && applied.state == model::TaskState::Review.as_str()
        && applied.phase == "verifying")
        .then(|| crate::review::verify(store, task_id));
    Ok(Started {
        reconciled,
        no_cost_warning,
        applied: Some(applied),
        verification,
    })
}

/// Runs one claimed attempt and waits for it: the wrapper child, then the result transaction.
/// `radian arc start` uses this; the scheduling loop uses [`spawn_claim`] and leaves the
/// result for its next pass (ADR 0048 decision 13).
pub fn run_claim(
    store: &Store,
    claim: &Claim,
    task: &model::TaskFile,
    config: &crate::store::config::Config,
) -> Result<Applied, Error> {
    let mut spawned = spawn_claim(store, claim, task, config)?;
    let status = spawned
        .child
        .wait()
        .map_err(|error| Error::Failed(format!("cannot wait for the wrapper: {error}")))?;
    apply_finished(store, claim, status.code())
}

/// One wrapper child this process started.
pub struct Spawned {
    pub child: Child,
}

/// Starts the wrapper for one claim, hands it the brief, moves the task to `doing.running`
/// and returns at once. The wrapper owns the attempt's limits from here (ADR 0048 decisions
/// 7.8 and 13).
pub fn spawn_claim(
    store: &Store,
    claim: &Claim,
    task: &model::TaskFile,
    config: &crate::store::config::Config,
) -> Result<Spawned, Error> {
    let executable = std::env::current_exe()
        .map_err(|error| Error::Failed(format!("cannot find the current executable: {error}")))?;
    spawn_claim_with(store, claim, task, config, &executable)
}

/// [`spawn_claim`] with the wrapper executable named, so a test can start a recorder in its
/// place and read the arguments the claim produced.
fn spawn_claim_with(
    store: &Store,
    claim: &Claim,
    task: &model::TaskFile,
    config: &crate::store::config::Config,
    executable: &std::path::Path,
) -> Result<Spawned, Error> {
    let task_id = claim.task.as_str();
    let (remote, main_branch) = store.repository_names()?;
    let run_dir = attempt_ops::run_dir(store, task_id, claim.attempt);
    fs::create_dir_all(&run_dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", run_dir.display())))?;
    let stdout_path = run_dir.join("wrapper.stdout.log");
    let stderr_path = run_dir.join("wrapper.stderr.log");

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
        .arg("--session")
        .arg(&claim.session_id)
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
    if claim.commit_dirty {
        command.arg("--commit-dirty");
    }
    let limits = Limits::of(task, config);
    command
        .arg("--stall-timeout-secs")
        .arg(limits.stall.as_secs().to_string())
        .arg("--max-wall-time-secs")
        .arg(limits.wall.as_secs().to_string())
        .arg("--cancel-grace-secs")
        .arg(limits.grace.as_secs().to_string());
    // The configured command worker or the radian worker's `exec_args`, from the claim's one
    // immutable worker line: a config change after the claim cannot start a different worker
    // than `start.json` records (ADR 0048 decisions 4 and 7).
    command.arg("--flow").arg(claim.worker.flow());
    add_worker_args(&mut command, &claim.worker)?;
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group: a terminal Ctrl-C reaches the scheduling loop only. The
        // wrapper handles a signal sent to it alone and leaves the worker bounded; the loop's
        // stop path leaves this worker running for the next pass to collect (ADR 0048
        // decision 13).
        command.process_group(0);
    }
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
    Ok(Spawned { child })
}

/// Adds the worker arguments to the wrapper's command: one JSON array on the hidden `--acp` or
/// `--command` flag, or the `radian exec` extra arguments after `--`. All come from the claim's
/// [`WorkerLine`], never from a configuration read after the claim.
fn add_worker_args(command: &mut Command, worker: &WorkerLine) -> Result<(), Error> {
    let flag = match worker {
        WorkerLine::Command { command: words, .. } => Some(("--command", words)),
        WorkerLine::Acp { acp, .. } => Some(("--acp", acp)),
        WorkerLine::Radian { .. } => None,
    };
    if let Some((flag, words)) = flag {
        let json = serde_json::to_string(words).map_err(|error| {
            Error::Failed(format!("cannot encode the flow's worker program: {error}"))
        })?;
        command.arg(flag).arg(json);
    }
    command.arg("--");
    if let WorkerLine::Radian { exec_args, .. } = worker {
        command.args(exec_args);
    }
    Ok(())
}

/// Reads what a finished wrapper left and applies it in one transaction (decision 9).
pub fn apply_finished(
    store: &Store,
    claim: &Claim,
    exit_code: Option<i32>,
) -> Result<Applied, Error> {
    let signal = attempt_ops::read_signal(store, &claim.task, claim.attempt);
    let finish = attempt_ops::read_finish(store, &claim.task, claim.attempt);
    attempt_ops::apply(
        store,
        &claim.task,
        claim.attempt,
        finish.as_ref(),
        signal,
        exit_code,
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

/// The reconcile step (decision 9, last bullet): for every task in `doing`, apply a result
/// that is there, leave a live attempt alone, and turn a dead attempt without a result into
/// an `infrastructure` (terminated) retry. It never starts a task, and it leaves
/// `doing.reconciling` for the owner.
///
/// Liveness comes from the wrapper's heartbeat, never from `ps`: the `workspace` sandbox
/// refuses `ps`, so a pass that ran inside one could not tell a dead wrapper from a live one
/// any other way.
pub fn reconcile(store: &Store) -> Result<Vec<Applied>, Error> {
    let config = store.config()?;
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
            // An old attempt's record is not this attempt's result: the heartbeat below
            // decides.
        }
        if attempt_is_live(store, &task, &config) {
            continue;
        }
        let result = attempt_ops::apply(store, &id, attempt, None, signal, None)?;
        if result.ignored.is_none() {
            applied.push(result);
        }
    }
    Ok(applied)
}

/// Whether the attempt's wrapper is still alive, from its heartbeat: a last beat older than
/// the attempt's stall window means dead. A claim whose wrapper has not beaten yet is judged
/// by the claim's own `start.json`, which it wrote in the same transaction as `doing`.
fn attempt_is_live(
    store: &Store,
    task: &model::TaskFile,
    config: &crate::store::config::Config,
) -> bool {
    let stall = task
        .stall_timeout_secs
        .unwrap_or(config.limits.stall_timeout_secs);
    let window = attempt_ops::liveness_window_secs(stall);
    if let Some(beat) = attempt_ops::read_heartbeat(store, &task.id, task.attempt) {
        return attempt_ops::utc_age_secs(&beat).is_some_and(|age| age <= window);
    }
    attempt_ops::read_start_time(store, &task.id, task.attempt)
        .and_then(|at| attempt_ops::utc_age_secs(&at))
        .is_some_and(|age| age <= window)
}

/// The wrapper's `(pid, start time)` from the attempt record. The start time is `None` when
/// `ps` could not report it (the `workspace` sandbox refuses `ps`).
fn wrapper_process(store: &Store, task: &str, attempt: u32) -> Option<(u32, Option<String>)> {
    let record = read_json(&attempt_ops::attempt_dir(store, task, attempt).join("identity.json"))?;
    Some((
        record.get("wrapper_pid")?.as_u64()? as u32,
        record
            .get("wrapper_process_started_at")
            .and_then(|value| value.as_str())
            .map(str::to_owned),
    ))
}

/// Whether the process the record names is still there. A recorded start time is compared, so
/// a reused pid is never signalled; without one, or when `ps` cannot report it (the sandbox
/// refused `ps`), the pid alone is the best identity arc has (ADR 0048 decision 9).
fn still_running(pid: u32, started: Option<&str>) -> bool {
    process::still_the_process(pid, started)
}

fn read_json(path: &std::path::Path) -> Option<serde_json::Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// `radian arc cancel <id>`: stop one task, or a whole plan (ADR 0048 decisions 3, 6 and 9).
///
/// The id resolves to a plan or a task as `reset-budget` resolves it: an id that names both is
/// refused (exit 3), and one that names neither is an error (exit 1).
pub fn cancel_any(store: &Store, id: &str) -> Result<Cancelled, Error> {
    match store.resolve_kind(id)? {
        crate::store::Kind::Plan => Ok(Cancelled::Plan(cancel_plan(store, id)?)),
        crate::store::Kind::Task => Ok(Cancelled::Task(Box::new(cancel(store, id)?))),
    }
}

/// `radian arc cancel <task>`: stop one task.
///
/// A `todo` task goes to `failed.cancelled`. A `doing` task gets SIGTERM on its wrapper (the
/// pid and its start time must match, so a reused pid is not signalled; when `ps` cannot
/// report the time, the pid alone decides, as ADR 0048 decision 9 says), then SIGKILL after
/// `[limits] cancel_grace_secs` plus [`CANCEL_RECORD_RESERVE`], the time the wrapper gets to
/// reap what it stopped and write its record. When the wrapper's `AttemptFinished` arrives the
/// task goes to `failed.cancelled`; when it never arrives the task goes to
/// `doing.reconciling`. A `review`
/// task's running gate is signalled first (the way the loop's own stop signals one), and the
/// task then goes to `failed.cancelled`. A task that ran out of attempts (`failed.exhausted`)
/// or cannot continue (`failed.irrecoverable`) goes to `failed.cancelled` as well: the owner
/// says "seen, given up". `done.*` and `failed.cancelled` are refused (exit 3).
pub fn cancel(store: &Store, task_id: &str) -> Result<Canceled, Error> {
    let _ = reconcile(store)?;
    let canceled = cancel_task(store, task_id)?;
    // The plan may have lost its last open task: arc proposes its review follow-up for the
    // merged tasks' items (ADR 0048 decision 10). A plan that cannot take the task (it changed
    // outside arc, or the task did not move all the way to `failed.cancelled`) is left as it
    // is: the cancel stands.
    if let Ok(loaded) = store.load_task(task_id) {
        let _ = crate::review::propose_followup(store, &loaded.item.plan);
    }
    Ok(canceled)
}

/// `radian arc cancel <plan>`: end every task `cancel` accepts in a plan and the plan itself.
///
/// In this order: the plan's `auto` becomes `off`; every task `cancel` accepts (a `todo`,
/// `doing` or `review` task, or one at `failed.exhausted` or `failed.irrecoverable`) is cancelled
/// by the task form's rules; a task in `done.*` or `failed.cancelled` is left alone; the plan's
/// `status` becomes `cancelled`. A task left at `doing.reconciling` does not stop the plan from
/// being cancelled. Once the plan is `cancelled` nothing can be added to it, so one more walk
/// ends any task that `add_task` added while the first walk ran.
pub fn cancel_plan(store: &Store, plan_id: &str) -> Result<PlanCanceled, Error> {
    let loaded = store.load_plan(plan_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(plan_id.to_owned()));
    }
    // The plan form refuses a plan that is cancelled by its tasks as it refuses one whose
    // `status` says so: nothing is left to cancel, and a cancelled plan is final.
    store.refuse_cancelled(plan_id)?;
    store.change_plan(plan_id, |plan| {
        // The stored status, re-read under the lock.
        if plan.is_cancelled() {
            return Err(crate::store::cancelled_plan(&plan.id));
        }
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    // One reconcile for the whole plan, then each task's cancel uses its current state.
    let _ = reconcile(store)?;
    let plan = store.load_plan(plan_id)?.item;
    let mut tasks = cancel_plan_tasks(store, &plan.tasks)?;
    store.change_plan(plan_id, |plan| {
        if plan.is_cancelled() {
            return Err(crate::store::cancelled_plan(&plan.id));
        }
        plan.status = "cancelled".to_owned();
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    // `add_task` reads the plan under `state.lock`, so a task it added between the walk and
    // this write is not in the snapshot above and would stay `todo.proposed` under a final
    // plan. It cannot add one now: the plan is `cancelled`. The second walk ends the ones
    // that made it in.
    let added: Vec<String> = store
        .load_plan(plan_id)?
        .item
        .tasks
        .into_iter()
        .filter(|id| !plan.tasks.contains(id))
        .collect();
    tasks.extend(cancel_plan_tasks(store, &added)?);
    Ok(PlanCanceled {
        plan: plan_id.to_owned(),
        tasks,
    })
}

/// Ends every task of `task_ids` that `cancel` accepts, by the task form's rules, and returns
/// the ones it changed.
fn cancel_plan_tasks(store: &Store, task_ids: &[String]) -> Result<Vec<Canceled>, Error> {
    let mut tasks = Vec::new();
    for task_id in task_ids {
        if !store.task_exists(task_id) {
            continue;
        }
        let task = store.load_task(task_id)?;
        // arc does not act on a file that changed outside it; the rest of the plan still ends.
        if task.changed_outside_arc {
            continue;
        }
        if !task.item.is_cancellable() {
            continue;
        }
        match cancel_task(store, task_id) {
            Ok(canceled) => tasks.push(canceled),
            // The task moved between the read and the cancel (a merge finished it, say): `done`
            // and `failed.cancelled` are left as they are.
            Err(Error::Refused(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(tasks)
}

/// One task's cancel, after the caller's reconcile: the task form of [`cancel_any`].
fn cancel_task(store: &Store, task_id: &str) -> Result<Canceled, Error> {
    cancel_task_once(store, task_id, true)
}

/// [`cancel_task`], with `again` naming whether a task the read found `todo` may be cancelled
/// once more when the transaction that would write `failed.cancelled` finds it moved instead
/// (a claim that took `state.lock` first left it `doing.starting`). The second pass cancels it
/// through the state it moved to; a task that moves again is refused rather than raced.
fn cancel_task_once(store: &Store, task_id: &str, again: bool) -> Result<Canceled, Error> {
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    match task.state {
        model::TaskState::Todo => cancel_todo(store, task_id, again),
        model::TaskState::Doing if task.phase == "reconciling" => {
            // The owner resolves a reconciling task with the same process check: only the
            // process the record names is stopped.
            let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
            stop_attempt_process(store, task_id, task.attempt, grace)?;
            store.change_task(task_id, |task| {
                if task.state == model::TaskState::Doing && task.phase == "reconciling" {
                    task.state = model::TaskState::Failed;
                    task.phase = "cancelled".to_owned();
                }
                Ok(())
            })?;
            let after = store.load_task(task_id)?.item;
            Ok(Canceled {
                task: task_id.to_owned(),
                applied: None,
                state: after.state.as_str().to_owned(),
                phase: after.phase,
            })
        }
        model::TaskState::Doing => cancel_running(store, task_id, &task),
        model::TaskState::Review => {
            // A gate the task is waiting on is stopped the way the loop's own stop stops it,
            // before the task moves (ADR 0048 decisions 9 and 10).
            if gate_running(store, task_id) {
                let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
                crate::gate::background::stop(store, task_id, grace);
            }
            store.change_task(task_id, |task| {
                if task.state != model::TaskState::Review {
                    return Err(cancel_refusal(task));
                }
                task.state = model::TaskState::Failed;
                task.phase = "cancelled".to_owned();
                task.retry_at = None;
                Ok(())
            })?;
            Ok(Canceled {
                task: task_id.to_owned(),
                applied: None,
                state: "failed".to_owned(),
                phase: "cancelled".to_owned(),
            })
        }
        model::TaskState::Failed if task.is_cancellable() => cancel_failed(store, task_id),
        model::TaskState::Done | model::TaskState::Failed => Err(cancel_refusal(&task)),
    }
}

/// The `failed` branch of the task form: a task that ran out of attempts (`failed.exhausted`)
/// or cannot continue (`failed.irrecoverable`) has no worker, so `cancel` only says "seen,
/// given up" and moves it to `failed.cancelled`. A stale gate record or lock, if one exists, is
/// stopped by the same process check the `review` branch makes; nothing is signalled blindly.
fn cancel_failed(store: &Store, task_id: &str) -> Result<Canceled, Error> {
    if gate_running(store, task_id) {
        let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
        crate::gate::background::stop(store, task_id, grace);
    }
    store.change_task(task_id, |task| {
        if task.state != model::TaskState::Failed || !task.is_cancellable() {
            return Err(cancel_refusal(task));
        }
        task.phase = "cancelled".to_owned();
        task.retry_at = None;
        Ok(())
    })?;
    Ok(Canceled {
        task: task_id.to_owned(),
        applied: None,
        state: "failed".to_owned(),
        phase: "cancelled".to_owned(),
    })
}

/// The one refusal text of the task form: the task's own state, then every phase `cancel`
/// accepts.
fn cancel_refusal(task: &model::TaskFile) -> Error {
    Error::Refused(format!(
        "task `{}` is `{}.{}`; only a `todo`, `doing` or `review` task, or a `failed.exhausted` or `failed.irrecoverable` one, can be cancelled",
        task.id,
        task.state.as_str(),
        task.phase
    ))
}

/// The `todo` branch: the write happens under the transaction's lock and only while the task
/// is still `todo`. A wrapper that was claimed in between took the attempt, so it must be
/// signalled rather than written over; the task is then cancelled through its new state.
fn cancel_todo(store: &Store, task_id: &str, again: bool) -> Result<Canceled, Error> {
    let mut moved = false;
    let written = store.change_task(task_id, |task| {
        if task.state != model::TaskState::Todo {
            moved = true;
            return Err(Error::Refused(format!(
                "task `{}` is `{}.{}`; it moved while it was being cancelled",
                task.id,
                task.state.as_str(),
                task.phase
            )));
        }
        task.state = model::TaskState::Failed;
        task.phase = "cancelled".to_owned();
        task.retry_at = None;
        Ok(())
    });
    match written {
        Ok(_) => Ok(Canceled {
            task: task_id.to_owned(),
            applied: None,
            state: "failed".to_owned(),
            phase: "cancelled".to_owned(),
        }),
        Err(_) if moved && again => cancel_task_once(store, task_id, false),
        Err(error) => Err(error),
    }
}

/// Stops the process an attempt record names, the way the task form of `cancel` stops one:
/// only the recorded pid and start time are signalled, SIGTERM, then SIGKILL after `grace`. A
/// record with no process, or one whose process is gone, does nothing.
///
/// `grace` is the caller's: the task form passes `[limits] cancel_grace_secs`, and the pass
/// that ends a cancelled plan's leftovers passes [`Duration::ZERO`], because the plan form
/// already sent that wrapper its SIGTERM and killed it.
fn stop_attempt_process(
    store: &Store,
    task_id: &str,
    attempt: u32,
    grace: Duration,
) -> Result<(), Error> {
    let Some((pid, start)) = wrapper_process(store, task_id, attempt) else {
        return Ok(());
    };
    if !still_running(pid, start.as_deref()) {
        return Ok(());
    }
    process::signal(pid, process::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && still_running(pid, start.as_deref()) {
        std::thread::sleep(POLL);
    }
    if still_running(pid, start.as_deref()) {
        process::signal(pid, process::SIGKILL);
    }
    Ok(())
}

/// The scheduling pass's step for a cancelled plan (ADR 0048 decisions 3 and 9): ends every
/// `doing.reconciling` task of a cancelled plan at `failed.cancelled`, after the process check
/// the task form makes. The plan is final, so the pass does not wait for the owner; without
/// this the task would stay in `doing` and count against `[limits] concurrency` for good.
///
/// Returns the tasks it ended, in id order. A task file that changed outside arc, and a task
/// of a plan that is not cancelled, are left alone.
pub fn end_reconciling_of_cancelled_plans(store: &Store) -> Result<Vec<String>, Error> {
    let mut ended = Vec::new();
    for id in store.task_ids()? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc
            || loaded.item.state != model::TaskState::Doing
            || loaded.item.phase != "reconciling"
            || !store.plan_is_cancelled(&loaded.item.plan)?
        {
            continue;
        }
        // The plan form already signalled this wrapper: the pass checks the process and
        // kills it if it is still there, but waits no second grace.
        stop_attempt_process(store, &id, loaded.item.attempt, Duration::ZERO)?;
        let mut changed = false;
        store.change_task(&id, |task| {
            if task.state == model::TaskState::Doing && task.phase == "reconciling" {
                task.state = model::TaskState::Failed;
                task.phase = "cancelled".to_owned();
                task.retry_at = None;
                changed = true;
            }
            Ok(())
        })?;
        if changed {
            ended.push(id);
        }
    }
    Ok(ended)
}

/// Whether a gate is running for the task: its record is `started` or `running`.
fn gate_running(store: &Store, task: &str) -> bool {
    crate::gate::background::read(store, task).is_some_and(|record| {
        matches!(
            record.status.as_str(),
            crate::gate::background::STARTED | crate::gate::background::RUNNING
        )
    })
}

fn cancel_running(store: &Store, task_id: &str, task: &model::TaskFile) -> Result<Canceled, Error> {
    let attempt = task.attempt;
    attempt_ops::write_signal(store, task_id, attempt, Class::Cancel)?;
    let config = store.config()?;
    let grace = Duration::from_secs(config.limits.cancel_grace_secs);
    if let Some((pid, start)) = wrapper_process(store, task_id, attempt)
        && still_running(pid, start.as_deref())
    {
        process::signal(pid, process::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline
            && attempt_ops::read_finish(store, task_id, attempt).is_none()
        {
            std::thread::sleep(POLL);
        }
        // The grace belongs to what the wrapper is stopping (the worker's group, a
        // `before_attempt` command's group): the wrapper only starts that group's own
        // SIGTERM-to-SIGKILL countdown when it observes the stop, and it still has to reap the
        // group and write its record. The extra window is for that work alone; taking it out
        // of the grace instead would shorten it, and a grace of one second or less to nothing.
        if attempt_ops::read_finish(store, task_id, attempt).is_none() {
            let deadline = Instant::now() + CANCEL_RECORD_RESERVE;
            while Instant::now() < deadline
                && attempt_ops::read_finish(store, task_id, attempt).is_none()
                && still_running(pid, start.as_deref())
            {
                std::thread::sleep(POLL);
            }
        }
        if attempt_ops::read_finish(store, task_id, attempt).is_none() {
            // The grace period ran out: SIGKILL, but only the process that is still the one
            // the record names.
            if still_running(pid, start.as_deref()) {
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
                task: task_id.to_owned(),
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
        task: task_id.to_owned(),
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
        .map(|applied| {
            if applied.model_wait {
                applied.line()
            } else {
                format!("reconciled: {}", applied.line())
            }
        })
        .collect();
    if let Some(warning) = &started.no_cost_warning {
        lines.push(warning.clone());
    }
    if let Some(applied) = &started.applied {
        lines.push(applied.line());
    }
    if let Some(Ok(verified)) = &started.verification {
        lines.push(verified.line());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::config;
    use crate::test_support::TempDir;

    /// A store over an empty repository: the layout's directories are created on the first
    /// write, and the config carries the defaults.
    fn store() -> (TempDir, Store) {
        let dir = TempDir::named("radian-arc-attempt");
        let store = Store::at(dir.path());
        fs::create_dir_all(store.root()).expect("the arc directory");
        fs::write(
            store.root().join(crate::store::CONFIG_FILE),
            config::default_text("main"),
        )
        .expect("config.toml");
        (dir, store)
    }

    fn todo_task(store: &Store) -> String {
        let plan = store
            .add_plan("plan", "A plan", "the request\n")
            .expect("a plan");
        store
            .add_task(
                &plan,
                "work",
                "A task",
                Vec::new(),
                false,
                Vec::new(),
                "brief\n",
            )
            .expect("a task")
    }

    fn state(store: &Store, task: &str) -> (String, String) {
        let task = store.load_task(task).expect("the task reads").item;
        (task.state.as_str().to_owned(), task.phase)
    }

    /// The wrapper's worker arguments come from the claim's immutable [`WorkerLine`], not from
    /// the configuration the caller happened to pass: a `config.toml` change between the load
    /// that precedes the claim and the claim itself cannot make `start.json` and the launched
    /// worker disagree (ADR 0048 decision 4).
    ///
    /// The claim is made with `worker.command` set; the config passed to the spawn has no
    /// command and another `exec_args`. The recorder must still see the claim's command.
    #[cfg(unix)]
    #[test]
    fn the_wrapper_starts_the_worker_the_claim_recorded() {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, store) = store();
        std::fs::create_dir_all(dir.path().join("tools")).expect("the tools directory");
        let program = dir.path().join("tools/worker");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").expect("the program");
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&program, permissions).unwrap();
        std::fs::write(
            store.root().join(crate::store::CONFIG_FILE),
            config::default_text("main").replace(
                "# command = [\"codex\", \"exec\"]",
                "command = [\"tools/worker\"]",
            ),
        )
        .expect("config with a command");
        let plan = store.add_plan("plan", "A plan", "the request\n").unwrap();
        let task_id = store
            .add_task(
                &plan,
                "work",
                "A task",
                Vec::new(),
                false,
                Vec::new(),
                "brief\n",
            )
            .unwrap();
        crate::store::plan_ops::confirm(&store, &plan).expect("confirm");
        let claim = attempt_ops::claim(&store, &task_id).expect("claim");
        let WorkerLine::Command { command, .. } = &claim.worker else {
            panic!("the claim recorded the configured command");
        };
        assert_eq!(
            command,
            &vec![dir.path().join("tools/worker").display().to_string()],
            "the claim resolved the program from the repository root"
        );

        // The configuration a different reader loaded afterwards: no command, other args.
        let mut later = config::Config::default();
        later.worker.exec_args = vec!["--model".to_owned(), "later".to_owned()];

        let recorded = dir.path().join("wrapper-args.txt");
        let recorder = dir.path().join("recorder.sh");
        std::fs::write(
            &recorder,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n",
                recorded.display()
            ),
        )
        .expect("the recorder");
        let mut permissions = std::fs::metadata(&recorder).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&recorder, permissions).unwrap();

        let task = store.load_task(&task_id).unwrap().item;
        let mut spawned =
            spawn_claim_with(&store, &claim, &task, &later, &recorder).expect("the wrapper starts");
        let _ = spawned.child.wait();

        let text = std::fs::read_to_string(&recorded).expect("the recorder wrote its arguments");
        let args: Vec<&str> = text.lines().collect();
        let at = args
            .iter()
            .position(|arg| *arg == "--command")
            .expect("the claim's command is on the wrapper line");
        assert_eq!(
            args[at + 1],
            serde_json::to_string(command).unwrap(),
            "the wrapper is started with the claim's command"
        );
        assert!(
            !args.contains(&"--model"),
            "the later configuration did not reach the wrapper: {args:?}"
        );
    }

    /// The `todo` branch writes `failed.cancelled` under the transaction's lock and only while
    /// the task is still `todo`. A claim that took the lock first leaves `doing.starting`: its
    /// wrapper is already starting, so the cancel must signal it rather than write over it, and
    /// the task ends where the `doing` path leaves it (`doing.reconciling` without a result).
    ///
    /// The task is moved before the branch runs, which is the state the transaction finds after
    /// a claim wins the race.
    #[test]
    fn a_todo_cancel_that_finds_a_claim_cancels_the_attempt_instead() {
        let (_dir, store) = store();
        let task = todo_task(&store);
        store
            .change_task(&task, |task| {
                task.state = model::TaskState::Doing;
                task.phase = "starting".to_owned();
                task.attempt = 1;
                Ok(())
            })
            .expect("the claim");

        let canceled = cancel_todo(&store, &task, true).expect("the moved task cancels");

        assert_eq!(
            (canceled.state.as_str(), canceled.phase.as_str()),
            ("doing", "reconciling"),
            "the cancel took the doing path, not the todo one"
        );
        assert_eq!(
            state(&store, &task),
            ("doing".to_owned(), "reconciling".to_owned()),
            "the claimed attempt was not overwritten with failed.cancelled"
        );
        assert_eq!(
            attempt_ops::read_signal(&store, &task, 1),
            Some(Class::Cancel),
            "the attempt's wrapper was signalled"
        );
    }

    /// A task that is still `todo` under the lock goes to `failed.cancelled`, as before.
    #[test]
    fn a_todo_cancel_that_finds_the_task_still_todo_writes_failed_cancelled() {
        let (_dir, store) = store();
        let task = todo_task(&store);

        let canceled = cancel_todo(&store, &task, true).expect("the todo task cancels");

        assert_eq!(
            (canceled.state.as_str(), canceled.phase.as_str()),
            ("failed", "cancelled")
        );
        assert_eq!(
            state(&store, &task),
            ("failed".to_owned(), "cancelled".to_owned())
        );
    }
}
