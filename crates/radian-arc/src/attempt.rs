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
    // The worker's command line is checked before anything is claimed or written: a wrong
    // `[worker] exec_args` entry would otherwise fail every attempt the same way, after the
    // claim (ADR 0048 decision 7).
    crate::worker_args::check(&store.config()?.worker.exec_args)?;
    let loaded = store.load_task(task_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(task_id.to_owned()));
    }
    let task = loaded.item;
    // A cancelled plan is final: none of its tasks starts or resumes verification (ADR 0048
    // decisions 3 and 9).
    let plan = store.load_plan(&task.plan)?;
    if plan.item.is_cancelled() {
        return Err(crate::store::cancelled_plan(&task.plan));
    }
    if task.state == model::TaskState::Review && task.phase == "verifying" {
        return Ok(Started {
            reconciled: Vec::new(),
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
    let task_id = claim.task.as_str();
    let (remote, main_branch) = store.repository_names()?;
    let run_dir = attempt_ops::run_dir(store, task_id, claim.attempt);
    fs::create_dir_all(&run_dir)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", run_dir.display())))?;
    let stdout_path = run_dir.join("wrapper.stdout.log");
    let stderr_path = run_dir.join("wrapper.stderr.log");

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
    let limits = Limits::of(task, config);
    command
        .arg("--stall-timeout-secs")
        .arg(limits.stall.as_secs().to_string())
        .arg("--max-wall-time-secs")
        .arg(limits.wall.as_secs().to_string())
        .arg("--cancel-grace-secs")
        .arg(limits.grace.as_secs().to_string());
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
/// a reused pid is never signalled; without one (the sandbox refused `ps`) the pid alone is
/// the best identity arc has.
fn still_running(pid: u32, started: Option<&str>) -> bool {
    match started.filter(|start| !start.is_empty()) {
        Some(start) => process::same_process(pid, start),
        None => process::alive(pid),
    }
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
/// pid and its start time must match, so a reused pid is never signalled), then SIGKILL after
/// `[limits] cancel_grace_secs`. When the wrapper's `AttemptFinished` arrives the task goes to
/// `failed.cancelled`; when it never arrives the task goes to `doing.reconciling`. A `review`
/// task's running gate is signalled first (the way the loop's own stop signals one), and the
/// task then goes to `failed.cancelled`.
pub fn cancel(store: &Store, task_id: &str) -> Result<Canceled, Error> {
    let _ = reconcile(store)?;
    cancel_task(store, task_id)
}

/// `radian arc cancel <plan>`: end every open task of a plan and the plan itself.
///
/// In this order: the plan's `auto` becomes `off`; every task in `todo`, `doing` or `review`
/// is cancelled by the task form's rules; a task in `done` or `failed` is left alone; the
/// plan's `status` becomes `cancelled`. A task left at `doing.reconciling` does not stop the
/// plan from being cancelled.
pub fn cancel_plan(store: &Store, plan_id: &str) -> Result<PlanCanceled, Error> {
    let loaded = store.load_plan(plan_id)?;
    if loaded.changed_outside_arc {
        return Err(Error::ChangedOutsideArc(plan_id.to_owned()));
    }
    if loaded.item.is_cancelled() {
        return Err(crate::store::cancelled_plan(plan_id));
    }
    store.change_plan(plan_id, |plan| {
        if plan.is_cancelled() {
            return Err(crate::store::cancelled_plan(&plan.id));
        }
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    // One reconcile for the whole plan, then each task's cancel uses its current state.
    let _ = reconcile(store)?;
    let plan = store.load_plan(plan_id)?.item;
    let mut tasks = Vec::new();
    for task_id in &plan.tasks {
        if !store.task_exists(task_id) {
            continue;
        }
        let task = store.load_task(task_id)?;
        // arc does not act on a file that changed outside it; the rest of the plan still ends.
        if task.changed_outside_arc {
            continue;
        }
        if !matches!(
            task.item.state,
            model::TaskState::Todo | model::TaskState::Doing | model::TaskState::Review
        ) {
            continue;
        }
        match cancel_task(store, task_id) {
            Ok(canceled) => tasks.push(canceled),
            // The task moved between the read and the cancel (a merge finished it, say):
            // `done` and `failed` are left as they are.
            Err(Error::Refused(_)) => {}
            Err(error) => return Err(error),
        }
    }
    store.change_plan(plan_id, |plan| {
        if plan.is_cancelled() {
            return Err(crate::store::cancelled_plan(&plan.id));
        }
        plan.status = "cancelled".to_owned();
        plan.auto = "off".to_owned();
        Ok(())
    })?;
    Ok(PlanCanceled {
        plan: plan_id.to_owned(),
        tasks,
    })
}

/// One task's cancel, after the caller's reconcile: the task form of [`cancel_any`].
fn cancel_task(store: &Store, task_id: &str) -> Result<Canceled, Error> {
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
                task: task_id.to_owned(),
                applied: None,
                state: "failed".to_owned(),
                phase: "cancelled".to_owned(),
            })
        }
        model::TaskState::Doing if task.phase == "reconciling" => {
            // The owner resolves a reconciling task with the same process check: only the
            // process the record names is stopped.
            stop_attempt_process(store, task_id, task.attempt)?;
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
                    return Err(Error::Refused(format!(
                        "task `{}` is `{}.{}`; only a `todo`, `doing` or `review` task can be cancelled",
                        task.id,
                        task.state.as_str(),
                        task.phase
                    )));
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
        model::TaskState::Done | model::TaskState::Failed => Err(Error::Refused(format!(
            "task `{task_id}` is `{}.{}`; only a `todo`, `doing` or `review` task can be cancelled",
            task.state.as_str(),
            task.phase
        ))),
    }
}

/// Stops the process an attempt record names, the way the task form of `cancel` stops one:
/// only the recorded pid and start time are signalled, SIGTERM, then SIGKILL after
/// `[limits] cancel_grace_secs`. A record with no process, or one whose process is gone, does
/// nothing.
fn stop_attempt_process(store: &Store, task_id: &str, attempt: u32) -> Result<(), Error> {
    let Some((pid, start)) = wrapper_process(store, task_id, attempt) else {
        return Ok(());
    };
    if !still_running(pid, start.as_deref()) {
        return Ok(());
    }
    let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
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
            || !store.load_plan(&loaded.item.plan)?.item.is_cancelled()
        {
            continue;
        }
        stop_attempt_process(store, &id, loaded.item.attempt)?;
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
