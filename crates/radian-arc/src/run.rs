//! `radian arc run`: the scheduling loop without a TUI (ADR 0048 decision 13).
//!
//! One `run` holds `runner.lock` for its lifetime and makes every transition it can make
//! now. A pass only schedules, and it ends in seconds; the work it starts runs outside the
//! pass and the next pass reads what it left (ADR 0048 decision 13). A pass, repeated until
//! one changes nothing:
//!
//! 1. reconcile every task in `doing` (apply a result that is there, leave a live attempt),
//!    and end every `doing.reconciling` task a plan cancel left behind at
//!    `failed.cancelled` (its plan is final);
//! 2. release every retry whose `retry_at` has passed;
//! 3. re-verify, without a worker, every task a `commands` or integration command failure
//!    blocked, once the remote main moved (ADR 0048 decision 9);
//! 4. start the gate of every task in `review.verifying`, or apply the verdicts a background
//!    gate left; this is also how a task a moved branch or a changed digest sent back reaches
//!    `review.awaiting_human` again;
//! 5. for every confirmed plan at `auto` `run` or `merge`, claim each `todo.ready` task whose
//!    `after` tasks are merged, up to `[limits] concurrency` attempts in flight at once, and
//!    start its wrapper without waiting for it;
//! 6. for every plan at `auto` `merge`, approve a verified task and start its integration
//!    gate, or finish the integration a gate left.
//!
//! Steps 4 to 6 walk the backlog in claim order: the plan's `priority` (`P0` first), then the
//! plan id, then the task's order in the plan. Priority only decides who gets a free
//! concurrency slot: a running attempt is never preempted.
//!
//! Waiting for a person, a future `retry_at`, a worker, or a gate does not keep it running:
//! the loop ends at the first pass that changes nothing. A one-shot `run` therefore exits
//! after starting work; `--watch` is how the loop keeps collecting it. A task of a plan at
//! `auto` `off` is never started or merged by `run`; its verification still advances.
//!
//! Every transition line is written through the caller's sink and flushed when the loop makes
//! it; the summary of what waits on whom prints when the pass ends. `watch` repeats a pass
//! after `--interval` seconds until SIGINT or SIGTERM, holds `runner.lock` the whole time, and
//! prints the summary only when it changed.
//!
//! Every line a loop prints also goes to `.radian/arc/loop.log`, each prefixed with an RFC 3339
//! UTC timestamp, flushed per line ([`LoopLog`]). The arc TUI reads that file for its ticker
//! and its log view (ADR 0048 decision 13).

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::store::attempt_ops::{self, Claim};
use crate::store::model::{self, TaskState};
use crate::store::{Error, PlanView, Store, plan_ops};

/// A hard stop on the pass count. Every pass that continues changes at least one task; this
/// only keeps a store that something else keeps rewriting from looping forever.
pub const MAX_PASSES: u32 = 1000;

/// The default wait between passes of `run --watch`, and the closed range it may have.
pub const DEFAULT_INTERVAL_SECS: u64 = 30;
pub const MIN_INTERVAL_SECS: u64 = 5;
pub const MAX_INTERVAL_SECS: u64 = 3600;

/// How often an `--interval` wait looks at the stop flag: a signal must end the loop promptly,
/// not after the whole interval.
const STOP_POLL: Duration = Duration::from_millis(100);

/// The log every `radian arc run` appends the lines it prints to (ADR 0048 decision 13).
/// It sits in `.radian/arc/` and carries one RFC 3339 UTC timestamp and a space before each
/// line.
pub const LOOP_LOG: &str = "loop.log";
/// The one old log a rotation keeps, in place of `loop.log`.
pub const LOOP_LOG_OLD: &str = "loop.log.1";
/// A `loop.log` at or above this size is rotated before the next line is written: the file
/// becomes `loop.log.1` (replacing an older one) and a new `loop.log` starts.
pub const LOOP_LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Set once when writing `loop.log` failed, so one process prints one line, not one per pass.
static LOG_WRITE_FAILED: AtomicBool = AtomicBool::new(false);

/// Set by the SIGINT/SIGTERM handler a `watch` loop installs.
static STOP: AtomicBool = AtomicBool::new(false);

/// What one `run` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// One line per transition, in the order the loop made them.
    pub transitions: Vec<String>,
    /// Why a task that could not advance waits; computed after the loop ran to quiet.
    pub waiting: Vec<String>,
    /// A refusal the loop met at quiet (a merge that cannot proceed, a verification that
    /// this build cannot run).
    pub notes: Vec<String>,
}

impl Report {
    /// The text `radian arc run` prints at the end of a pass: the summary, with `no transition`
    /// when the pass wrote none. The transition lines are written as they happen, through the
    /// sink [`run`] and [`watch`] take.
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        if self.transitions.is_empty() {
            out.push_str("no transition\n");
        }
        out.push_str("summary\n");
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        for line in &self.waiting {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Runs one pass to quiet, holding `runner.lock` for its lifetime. `emit` receives each
/// transition line with its newline, as the loop makes it, and then the summary. Every line is
/// also appended to [`LOOP_LOG`].
///
/// # Errors
///
/// Another process holds `runner.lock` ([`Error::Locked`], exit 4), a file arc needs cannot be
/// read or written, or the store did not settle within [`MAX_PASSES`] passes.
pub fn run(store: &Store, emit: &mut dyn FnMut(&str)) -> Result<Report, Error> {
    let mut log = LoopLog::new(store);
    let lock = take_lock(store, &mut log)?;
    let mut sink = |text: &str| {
        log.write(text);
        emit(text);
    };
    let report = match run_quiet(store, &mut sink, None) {
        Ok(report) => report,
        Err(error) => {
            sink(&format!("radian arc: {}\n", error.message()));
            return Err(error);
        }
    };
    sink(&report.render_summary());
    drop(lock);
    Ok(report)
}

/// Runs a pass, then waits `interval_secs` and runs another, until SIGINT or SIGTERM. Holds
/// `runner.lock` for the whole loop. `emit` receives each transition line with its newline as
/// it happens, and a pass's summary block when it differs from the last one printed.
///
/// A signal starts no new work: a running worker and a running gate are separate processes.
/// A worker is left alone (the next pass, or the next `run`, collects it); a running gate is
/// signalled and gets `[limits] cancel_grace_secs` before SIGKILL, so the next loop starts it
/// again as unverified. The loop then prints the summary, releases the lock and returns
/// [`Exit`](crate::store::Exit) `0` through the caller.
///
/// # Errors
///
/// The same as [`run`].
pub fn watch(store: &Store, interval_secs: u64, emit: &mut dyn FnMut(&str)) -> Result<(), Error> {
    let mut log = LoopLog::new(store);
    let lock = take_lock(store, &mut log)?;
    let mut sink = |text: &str| {
        log.write(text);
        emit(text);
    };
    STOP.store(false, Ordering::SeqCst);
    catch_stop_signals();
    let mut last: Option<String> = None;
    loop {
        if stop_requested() {
            break;
        }
        let report = match run_quiet(store, &mut sink, Some(&STOP)) {
            Ok(report) => report,
            // One task's error does not end the loop: print it and run the next pass. The
            // lock and every state file stay as they are; a one-shot `run` still returns
            // the error and exits non-zero.
            Err(error) => {
                sink(&format!("radian arc: {}\n", error.message()));
                if stop_requested() || !wait_interval(interval_secs) {
                    break;
                }
                continue;
            }
        };
        let summary = report.render_summary();
        if last.as_deref() != Some(summary.as_str()) {
            sink(&summary);
            last = Some(summary);
        }
        if stop_requested() || !wait_interval(interval_secs) {
            break;
        }
    }
    if stop_requested() {
        stop_gates(store)?;
    }
    drop(lock);
    Ok(())
}

/// Stops the gates a loop leaves running when it stops: a short grace, then SIGKILL. Each
/// gate arc stopped records `interrupted`, so the next loop starts it again and counts no
/// death.
pub fn stop_gates(store: &Store) -> Result<(), Error> {
    let grace = Duration::from_secs(store.config()?.limits.cancel_grace_secs);
    for id in store.task_ids()? {
        crate::gate::background::stop(store, &id, grace);
    }
    Ok(())
}

/// The writer of `.radian/arc/loop.log`: every line a scheduling loop prints, prefixed with an
/// RFC 3339 UTC timestamp and a space, appended and flushed as it prints. A log at or above
/// [`LOOP_LOG_MAX_BYTES`] becomes `loop.log.1` (replacing an older one) before the next line,
/// so one old file is kept. A write error prints one `radian arc:` line for the whole process
/// and the loop goes on.
pub struct LoopLog {
    path: PathBuf,
}

impl LoopLog {
    /// The log of `store`'s repository.
    #[must_use]
    pub fn new(store: &Store) -> Self {
        Self {
            path: store.root().join(LOOP_LOG),
        }
    }

    /// Appends `text`: one timestamped line per line of `text`, each flushed.
    pub fn write(&mut self, text: &str) {
        for line in text.lines() {
            self.write_line(line);
        }
    }

    /// The log's path.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn write_line(&mut self, line: &str) {
        self.rotate_if_needed();
        let stamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut file| {
                writeln!(file, "{stamp} {line}")?;
                file.flush()
            });
        if let Err(error) = result {
            self.report(error);
        }
    }

    /// Rotates a full log before the line that would grow it past the limit.
    fn rotate_if_needed(&mut self) {
        let full = std::fs::metadata(&self.path).is_ok_and(|meta| meta.len() >= LOOP_LOG_MAX_BYTES);
        if !full {
            return;
        }
        let old = self.path.with_file_name(LOOP_LOG_OLD);
        let result = std::fs::remove_file(&old)
            .or_else(|error| match error.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(error),
            })
            .and_then(|()| std::fs::rename(&self.path, &old));
        if let Err(error) = result {
            self.report(error);
        }
    }

    /// One line per process, as decision 13 asks; a loop that cannot write its log still runs.
    fn report(&self, error: std::io::Error) {
        if !LOG_WRITE_FAILED.swap(true, Ordering::SeqCst) {
            eprintln!("radian arc: cannot write {}: {error}", self.path.display());
        }
    }
}

fn take_lock(store: &Store, log: &mut LoopLog) -> Result<crate::store::RunnerLock, Error> {
    match store.try_runner_lock()? {
        Some(lock) => Ok(lock),
        None => {
            let error = Error::Locked(crate::store::RUNNER_LOCK);
            // The line `radian arc run` prints for this is part of what the TUI's loop.log
            // reader may name when a loop it started exited 4.
            log.write(&format!("radian arc: {}\n", error.message()));
            Err(error)
        }
    }
}

/// The passes of one `run` or one `watch` pass, until a pass changes nothing. When `stop` is
/// set, no further pass starts: work already running inside a pass still ends normally.
fn run_quiet(
    store: &Store,
    emit: &mut dyn FnMut(&str),
    stop: Option<&AtomicBool>,
) -> Result<Report, Error> {
    let mut report = Report::default();
    let mut passes = 0;
    loop {
        if stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
            break;
        }
        passes += 1;
        if passes > MAX_PASSES {
            return Err(Error::Failed(format!(
                "the scheduling loop did not settle within {MAX_PASSES} passes"
            )));
        }
        let changed = {
            let mut transitions = Transitions {
                report: &mut report,
                emit: &mut *emit,
            };
            one_pass(store, &mut transitions, stop)?
        };
        if !changed {
            break;
        }
    }
    report.waiting = waiting(store)?;
    Ok(report)
}

/// The transitions of one pass: their text goes to the caller's sink at once, and the report
/// keeps them for [`Report::render_summary`]'s `no transition` line.
struct Transitions<'a> {
    report: &'a mut Report,
    emit: &'a mut dyn FnMut(&str),
}

impl Transitions<'_> {
    fn push(&mut self, line: String) {
        (self.emit)(&format!("{line}\n"));
        self.report.transitions.push(line);
    }

    fn note(&mut self, line: String) {
        self.report.notes.push(line);
    }
}

/// Installs the SIGINT and SIGTERM handler of a watch loop. The handler only stores to an
/// atomic, which is async-signal-safe.
fn catch_stop_signals() {
    // SAFETY: `request_stop` has the C ABI `signal` expects, and its body only stores to an
    // `AtomicBool`. The previous handler is not needed.
    unsafe {
        libc::signal(
            libc::SIGINT,
            request_stop as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            request_stop as *const () as libc::sighandler_t,
        );
    }
}

extern "C" fn request_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Waits `interval_secs` in [`STOP_POLL`] slices. `false` when a stop signal arrived first.
fn wait_interval(interval_secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(interval_secs);
    loop {
        if stop_requested() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(STOP_POLL));
    }
}

/// One pass. `true` when it changed a stored file, so the loop takes another pass. `stop`
/// (the `--watch` signal flag) keeps the pass from starting an attempt or a merge once it is
/// set; work already running when it was set still ends normally.
fn one_pass(
    store: &Store,
    transitions: &mut Transitions,
    stop: Option<&AtomicBool>,
) -> Result<bool, Error> {
    // Every pass that could claim a task checks the worker's command line first, so a
    // rejected line refuses `run` before its first pass changes anything (ADR 0048
    // decision 7).
    crate::worker_args::check(&store.config()?.worker.exec_args)?;
    let mut changed = false;

    // 1. Doing: apply a finished attempt, leave a live one alone.
    for applied in crate::attempt::reconcile(store)? {
        transitions.push(format!("reconciled: {}", applied.line()));
        changed = true;
    }

    // 1b. A task a plan cancel left at `doing.reconciling` stays in `doing` otherwise, and
    // counts against the concurrency limit for good. The plan is final, so the pass ends the
    // task instead of waiting for the owner (ADR 0048 decisions 3 and 9).
    for id in crate::attempt::end_reconciling_of_cancelled_plans(store)? {
        transitions.push(format!(
            "{id} doing.reconciling -> failed.cancelled (its plan is cancelled)"
        ));
        changed = true;
    }

    // 2. A retry whose time has come.
    for id in attempt_ops::release_retries(store, &plan_ops::utc_now())? {
        transitions.push(format!("{id} released: todo.retry_wait -> todo.ready"));
        changed = true;
    }

    // 3. Automatic reverify, for a task a command failure blocked once main moved (ADR 0048
    // decision 9). The claim is worker-less; step 4 starts its gate in this same pass.
    if !stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        changed |= auto_reverify_tasks(store, transitions)?;
    }

    // 4. Verification, for every plan: a plan at `auto` `off` still advances here. The gate
    // runs as a background process; this pass applies what an earlier gate left (ADR 0048
    // decision 10).
    for id in verifying_tasks(store)? {
        changed |= advance_verification(store, &id, transitions, stop)?;
    }

    // 5. Start what the auto levels allow, and 6. merge what passed, unless a stop signal
    // arrived while steps 1-4 ran: no new attempt and no new gate starts after it.
    if !stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        changed |= start_ready(store, transitions)?;
        changed |= merge_auto(store, transitions, stop)?;
    }

    Ok(changed)
}

/// One task in `review.verifying`: apply the verdicts a background gate left, or start its
/// gate. `true` when it moved the task (or started something that will).
fn advance_verification(
    store: &Store,
    task_id: &str,
    transitions: &mut Transitions,
    stop: Option<&AtomicBool>,
) -> Result<bool, Error> {
    let task = store.load_task(task_id)?.item;
    let label = task
        .attempt_id
        .clone()
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| format!("{}-{}", task.id, task.attempt));
    // A plan this pass could reuse when the unchanged refusal or no-record record lets it
    // start nothing; planning costs a fetch, so it happens only when a start is possible.
    let mut planned: Option<crate::review::Verification> = None;
    // How many deaths this attempt and these inputs already have (ADR 0048 decision 10).
    let mut deaths = 0;
    let record = crate::gate::background::read(store, task_id)
        .filter(|record| record.kind == "verify" && record.attempt == label);
    if let Some(record) = &record {
        match record.status.as_str() {
            crate::gate::background::STARTED => {
                // The pass wrote the inputs it started the gate with and no gate record came
                // back. A held lock means the gate is still coming up; a free one means the
                // process exited without writing anything, and another start with the same
                // inputs would only repeat it (ADR 0048 decision 10).
                if !crate::gate::background::running(store, task_id)
                    && let Some(inputs) = record.inputs.clone()
                {
                    crate::gate::background::write_no_record(
                        store, task_id, "verify", &label, &inputs,
                    )?;
                    transitions.note(format!(
                        "{task_id} verify cannot run: {}",
                        crate::gate::background::NO_RECORD_TEXT
                    ));
                    return Ok(false);
                }
            }
            // A gate that wrote `running` and is gone died; arc's own stop (`interrupted`)
            // is restarted below without counting (ADR 0048 decision 10).
            crate::gate::background::RUNNING => {
                if !crate::gate::background::running(store, task_id) {
                    let planned_now = crate::review::plan_verification(store, task_id).ok();
                    match crate::gate::background::count_death(
                        store,
                        task_id,
                        "verify",
                        &label,
                        record,
                        planned_now.as_ref().map(|plan| plan.inputs.as_str()),
                    )? {
                        crate::gate::background::Death::Stop => {
                            transitions.note(format!(
                                "{task_id} verify cannot run: {}",
                                crate::gate::background::DIED_TEXT
                            ));
                            return Ok(false);
                        }
                        crate::gate::background::Death::Count(count) => {
                            deaths = count;
                            planned = planned_now;
                        }
                        crate::gate::background::Death::Restart => {}
                    }
                }
            }
            "error" => {
                crate::gate::background::clear(store, task_id);
                return Err(Error::Failed(
                    record
                        .text
                        .clone()
                        .unwrap_or_else(|| format!("the gate of {task_id} failed")),
                ));
            }
            // Reported once, and no new gate for this attempt while the inputs the gate
            // saw are unchanged: a repeat would rerun the same refusal (or the same missing
            // record) every interval. A new attempt, a changed config, or the branch coming
            // back changes the inputs (ADR 0048 decision 10).
            crate::gate::background::NO_RECORD
            | crate::gate::background::DIED
            | crate::gate::background::REFUSED => {
                match crate::review::plan_verification(store, task_id) {
                    Ok(plan) if record.inputs.as_deref() == Some(plan.inputs.as_str()) => {
                        let text = record
                            .text
                            .clone()
                            .unwrap_or_else(|| crate::gate::background::NO_RECORD_TEXT.to_owned());
                        transitions.note(format!("{task_id} verify cannot run: {text}"));
                        return Ok(false);
                    }
                    // The inputs changed: start a gate below.
                    Ok(plan) => planned = Some(plan),
                    Err(Error::Refused(reason)) => {
                        transitions.note(format!("{task_id} verify cannot run: {reason}"));
                        return Ok(false);
                    }
                    Err(error) => return Err(error),
                }
            }
            // arc stopped the gate itself: the next pass starts it again at once and counts
            // no death (ADR 0048 decision 10).
            crate::gate::background::INTERRUPTED => {}
            "done" => {
                crate::gate::background::clear(store, task_id);
                return match crate::review::apply_verification(store, task_id) {
                    Ok(verified) => {
                        transitions.push(verified.line());
                        let mut changed = true;
                        // A review rework already counted itself and may have paused the plan;
                        // every other failure to `rework_pending` counts here (ADR 0048
                        // decisions 9 and 12). A conflict that reached its own limit is never
                        // an automatic rework: it does not pause the plan.
                        let paused = match &verified.paused {
                            Some(paused) => Some(paused.clone()),
                            None if verified.phase == "rework_pending"
                                && verified.conflict.is_none() =>
                            {
                                attempt_ops::record_auto_rework(store, task_id)?
                            }
                            None => None,
                        };
                        if let Some(paused) = paused {
                            transitions.push(format!("{} paused: {}", paused.plan, paused.text));
                            changed = true;
                        }
                        Ok(changed)
                    }
                    // The branch moved or a stage this build does not run: the task stays in
                    // `review.verifying` and the reason is recorded; the summary reports it.
                    Err(Error::Refused(reason)) => {
                        transitions.note(format!("{task_id} verify cannot run: {reason}"));
                        Ok(false)
                    }
                    Err(error) => Err(error),
                };
            }
            _ => {}
        }
    }
    // A gate that is still running is the reason this pass starts none.
    if crate::gate::background::running(store, task_id) {
        return Ok(false);
    }
    if stop.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Ok(false);
    }
    // The digest of what the gate will read, written before it starts: a gate that exits
    // without a record leaves it for the next pass (ADR 0048 decision 10). A plan that
    // refuses is the gate's own record to write; there is no digest to guard.
    let planned = match planned {
        Some(plan) => Some(plan),
        None => crate::review::plan_verification(store, task_id).ok(),
    };
    match crate::gate::background::start(
        store,
        task_id,
        &crate::gate::background::Start {
            kind: "verify",
            attempt: label.clone(),
            inputs: planned.map(|plan| plan.inputs),
            deaths,
        },
    )? {
        Some(pid) => {
            transitions.push(format!("{label} verify started (pid {pid})"));
            Ok(true)
        }
        // One gate per task: the running one is the reason this pass starts none.
        None => Ok(false),
    }
}

/// Re-verifies by itself every task a moved main unblocked (ADR 0048 decision 9): a
/// `review.rework_pending` task of a plan whose `auto` is not `off`, whose `commands` or
/// integration stage failed with `command_failed` or `timeout`, once the remote main is no
/// longer the main that verdict recorded. `true` when it claimed an attempt or reported the
/// count stop.
fn auto_reverify_tasks(store: &Store, transitions: &mut Transitions) -> Result<bool, Error> {
    let mut changed = false;
    for id in rework_pending_tasks(store)? {
        match crate::review::auto_reverify(store, &id)? {
            crate::review::AutoReverify::Not => {}
            crate::review::AutoReverify::Started { main_sha, .. } => {
                transitions.push(format!(
                    "{id} main moved ({}) -> auto reverify",
                    short_sha(&main_sha)
                ));
                changed = true;
            }
            crate::review::AutoReverify::Limit { main_sha, text } => {
                // The count is used up: `run` prints the reason, `show` prints the same text
                // (ADR 0048 decision 9).
                transitions.push(format!(
                    "{id} main moved ({}) -> auto reverify limit reached: {text}",
                    short_sha(&main_sha)
                ));
                changed = true;
            }
        }
    }
    Ok(changed)
}

/// The first seven characters of a sha.
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// Every plan in claim order: the owner's `priority` (`P0` first), then the plan id. A plan
/// file that changed outside arc keeps its place; the caller skips it where it must not act.
fn plans_in_claim_order(store: &Store) -> Result<Vec<PlanView>, Error> {
    let mut views = store.plans()?;
    views.sort_by(|one, other| model::claim_order(&one.plan.item, &other.plan.item));
    Ok(views)
}

/// Every task in claim order: its plan's priority, then the plan id, then the task's position
/// in the plan. A task file no plan lists sorts last, by id.
fn task_claim_order(store: &Store) -> Result<Vec<String>, Error> {
    let mut keys: std::collections::HashMap<String, (usize, String, usize)> =
        std::collections::HashMap::new();
    for view in plans_in_claim_order(store)? {
        let rank = model::priority_rank(view.plan.item.priority());
        for (index, task) in view.tasks.iter().enumerate() {
            keys.entry(task.item.id.clone())
                .or_insert_with(|| (rank, view.plan.item.id.clone(), index));
        }
    }
    let key = |id: &str| {
        keys.get(id)
            .cloned()
            .unwrap_or((usize::MAX, String::new(), 0))
    };
    let mut ids = store.task_ids()?;
    ids.sort_by(|one, other| key(one).cmp(&key(other)).then_with(|| one.cmp(other)));
    Ok(ids)
}

/// Whether the task's plan is `cancelled`: the loop neither claims nor gates its tasks
/// (ADR 0048 decisions 3 and 13).
fn plan_is_cancelled(store: &Store, plan: &str) -> Result<bool, Error> {
    Ok(store.load_plan(plan)?.item.is_cancelled())
}

/// Every task in `review.rework_pending`, in claim order. A file that changed outside arc and
/// a task of a `cancelled` plan are skipped.
fn rework_pending_tasks(store: &Store) -> Result<Vec<String>, Error> {
    let mut ids = Vec::new();
    for id in task_claim_order(store)? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc || plan_is_cancelled(store, &loaded.item.plan)? {
            continue;
        }
        if loaded.item.state == TaskState::Review && loaded.item.phase == "rework_pending" {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Every task in `review.verifying`, in claim order. A file that changed outside arc and a task
/// of a `cancelled` plan are skipped.
fn verifying_tasks(store: &Store) -> Result<Vec<String>, Error> {
    let mut ids = Vec::new();
    for id in task_claim_order(store)? {
        let loaded = store.load_task(&id)?;
        if loaded.changed_outside_arc || plan_is_cancelled(store, &loaded.item.plan)? {
            continue;
        }
        if loaded.item.state == TaskState::Review && loaded.item.phase == "verifying" {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// Claims and runs ready tasks of plans at `auto` `run` or `merge`, in claim order: the plan's
/// priority, then the plan id, then the task order. Priority picks who gets a free slot; it
/// never preempts an attempt that is already running. `true` when it ran at least one attempt.
fn start_ready(store: &Store, transitions: &mut Transitions) -> Result<bool, Error> {
    let config = store.config()?;
    let concurrency = u64::from(config.limits.concurrency.max(1));
    let doing = count_doing(store)?;
    if doing >= concurrency {
        return Ok(false);
    }
    let mut slots = concurrency - doing;
    let mut claims: Vec<(Claim, model::TaskFile)> = Vec::new();
    for view in plans_in_claim_order(store)? {
        if slots == 0 {
            break;
        }
        if view.plan.changed_outside_arc
            || view.plan.item.status != "confirmed"
            || !matches!(view.plan.item.auto.as_str(), "run" | "merge")
        {
            continue;
        }
        for task in &view.tasks {
            if slots == 0 {
                break;
            }
            if task.changed_outside_arc
                || task.item.state != TaskState::Todo
                || task.item.phase != "ready"
                || !after_merged(store, &task.item)?
            {
                continue;
            }
            match attempt_ops::claim(store, &task.item.id) {
                Ok(claim) => {
                    slots -= 1;
                    claims.push((claim, task.item.clone()));
                }
                // The claim refused: another writer took the slot or the task moved. The
                // next pass re-reads everything.
                Err(Error::Refused(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    if claims.is_empty() {
        return Ok(false);
    }
    // A claim only starts the wrapper: the wrapper owns the attempt's limits from here, and
    // this pass does not wait for it (ADR 0048 decisions 7.8 and 13). A thread waits for the
    // child so a long `--watch` collects no zombie; the thread dies with this process, and
    // the worker keeps running either way.
    let config = store.config()?;
    for (claim, task) in &claims {
        let spawned = crate::attempt::spawn_claim(store, claim, task, &config)?;
        let mut child = spawned.child;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        transitions.push(format!("{} started -> doing.running", claim.attempt_id));
    }
    Ok(true)
}

/// Merges every finished task of a plan at `auto` `merge`, in claim order. `true` when a task
/// changed state.
fn merge_auto(
    store: &Store,
    transitions: &mut Transitions,
    stop: Option<&AtomicBool>,
) -> Result<bool, Error> {
    let mut changed = false;
    for view in plans_in_claim_order(store)? {
        if view.plan.changed_outside_arc
            || view.plan.item.status != "confirmed"
            || view.plan.item.auto != "merge"
        {
            continue;
        }
        for task in &view.tasks {
            if task.changed_outside_arc || task.item.state != TaskState::Review {
                continue;
            }
            let phase = task.item.phase.as_str();
            if !matches!(phase, "awaiting_human" | "integrating" | "ready_to_merge") {
                continue;
            }
            let stopped = stop.is_some_and(|flag| flag.load(Ordering::SeqCst));
            match crate::review::merge_step(store, &task.item.id, stopped) {
                Ok(step) => {
                    if let Some(line) = step.line {
                        transitions.push(line);
                        changed = true;
                    }
                    if let Some(note) = step.note {
                        transitions.note(note);
                    }
                    let after = store.load_task(&task.item.id)?.item;
                    if after.state == TaskState::Review
                        && after.phase == "rework_pending"
                        && let Some(paused) = attempt_ops::record_auto_rework(store, &task.item.id)?
                    {
                        transitions.push(format!("{} paused: {}", paused.plan, paused.text));
                        changed = true;
                    }
                }
                Err(Error::Refused(reason)) => {
                    // A moved branch and a changed digest both return the task to
                    // `review.verifying`; that is a transition and the next pass verifies.
                    let after = store.load_task(&task.item.id)?.item;
                    if after.state != TaskState::Review || after.phase != phase {
                        transitions.push(format!(
                            "{} merge -> {}.{}: {reason}",
                            task.item.id,
                            after.state.as_str(),
                            after.phase
                        ));
                        changed = true;
                    } else {
                        // The state allows no merge and nothing moved: the summary says why
                        // instead of the loop spinning on it.
                        transitions.note(format!("{} cannot merge: {reason}", task.item.id));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(changed)
}

/// Whether every task in `after` is `done.merged`.
fn after_merged(store: &Store, task: &model::TaskFile) -> Result<bool, Error> {
    for id in &task.after {
        let loaded = store.load_task(id)?;
        if loaded.changed_outside_arc
            || loaded.item.state != TaskState::Done
            || loaded.item.phase != "merged"
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How many tasks are in `doing`.
fn count_doing(store: &Store) -> Result<u64, Error> {
    let mut count = 0;
    for id in store.task_ids()? {
        if store.load_task(&id)?.item.state == TaskState::Doing {
            count += 1;
        }
    }
    Ok(count)
}

/// The final summary: for each plan and task that is not merged, what it waits on. Plans are
/// listed in claim order.
fn waiting(store: &Store) -> Result<Vec<String>, Error> {
    let mut lines = Vec::new();
    for view in plans_in_claim_order(store)? {
        let plan = &view.plan.item;
        // A cancelled plan waits on nothing: the loop never serves it again.
        if plan.is_cancelled() {
            continue;
        }
        let open: Vec<&crate::store::Loaded<model::TaskFile>> = view
            .tasks
            .iter()
            .filter(|task| !(task.item.state == TaskState::Done && task.item.phase == "merged"))
            .collect();
        if open.is_empty() && !view.plan.changed_outside_arc {
            continue;
        }
        let header = if view.plan.changed_outside_arc {
            format!(
                "{} (changed outside arc; run `radian arc adopt {}`)",
                plan.id, plan.id
            )
        } else {
            format!("{} (auto {})", plan.id, plan.auto)
        };
        lines.push(header);
        for task in open {
            let reason = task_reason(store, plan, task)?;
            lines.push(format!(
                "  {} {}.{}: {reason}",
                task.item.id,
                task.item.state.as_str(),
                task.item.phase
            ));
        }
    }
    if lines.is_empty() {
        lines.push("nothing waits".to_owned());
    }
    Ok(lines)
}

fn task_reason(
    store: &Store,
    plan: &model::PlanFile,
    task: &crate::store::Loaded<model::TaskFile>,
) -> Result<String, Error> {
    let item = &task.item;
    let reason = match (item.state, item.phase.as_str()) {
        (TaskState::Todo, "ready") => {
            if plan.auto == "off" {
                "waiting for you to start it (`auto` is `off`)".to_owned()
            } else {
                let missing: Vec<&str> = item
                    .after
                    .iter()
                    .filter(|id| {
                        store.load_task(id).is_ok_and(|loaded| {
                            loaded.item.state != TaskState::Done || loaded.item.phase != "merged"
                        })
                    })
                    .map(String::as_str)
                    .collect();
                if missing.is_empty() {
                    "waiting for a free concurrency slot".to_owned()
                } else {
                    format!("waiting for {} to merge", missing.join(", "))
                }
            }
        }
        (TaskState::Todo, "retry_wait") => format!(
            "waiting for its retry at {}",
            item.retry_at.as_deref().unwrap_or("an unrecorded time")
        ),
        (TaskState::Todo, "paused_budget") => "waiting for `radian arc reset-budget`".to_owned(),
        (TaskState::Todo, phase) if phase == model::PROPOSED_PHASE => {
            "waiting for you to confirm it (`radian arc confirm`)".to_owned()
        }
        (TaskState::Todo, _) => "waiting for its `after` tasks".to_owned(),
        (TaskState::Doing, "starting" | "running") => "running its attempt".to_owned(),
        (TaskState::Doing, _) => "needs you: arc cannot prove what the worker did".to_owned(),
        (TaskState::Review, "verifying") => match crate::review::rework_reason(store, item) {
            Some(reason) => format!("verification cannot run: {reason}"),
            None => "waiting to be verified".to_owned(),
        },
        (TaskState::Review, "rework_pending") => match crate::review::rework_reason(store, item) {
            Some(reason) => format!("waiting for a rework brief from the lead ({reason})"),
            None => "waiting for a rework brief from the lead".to_owned(),
        },
        (TaskState::Review, "awaiting_human") => {
            if plan.auto == "merge" {
                "waiting: its verification passed".to_owned()
            } else {
                "waiting for you to merge or reject it".to_owned()
            }
        }
        (TaskState::Review, "integrating" | "ready_to_merge") => {
            "waiting for its integration to finish".to_owned()
        }
        (TaskState::Review, _) => "waiting in review".to_owned(),
        (TaskState::Failed, phase) => format!("failed.{phase}; nothing starts it again"),
        (TaskState::Done, phase) => format!("done.{phase}"),
    };
    Ok(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_no_transition_says_so_and_lists_the_summary() {
        let report = Report {
            transitions: Vec::new(),
            waiting: vec!["nothing waits".to_owned()],
            notes: Vec::new(),
        };
        assert_eq!(
            report.render_summary(),
            "no transition\nsummary\nnothing waits\n"
        );
    }

    /// The transitions were already written when they happened; the summary does not repeat
    /// them.
    #[test]
    fn a_summary_after_a_transition_holds_only_the_summary() {
        let report = Report {
            transitions: vec!["TK0001-work-1 completed -> review.verifying".to_owned()],
            waiting: vec!["nothing waits".to_owned()],
            notes: Vec::new(),
        };
        assert_eq!(report.render_summary(), "summary\nnothing waits\n");
    }
}
