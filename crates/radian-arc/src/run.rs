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
//! after starting work; `radian arc serve` is how the loop keeps collecting it. A task of a
//! plan at `auto` `off` is never started or merged by `run`; its verification still advances.
//!
//! Every transition line is written through the caller's sink and flushed when the loop makes
//! it; the summary of what waits on whom prints when the pass ends. `serve` repeats a pass
//! after its interval seconds until SIGINT or SIGTERM, holds `runner.lock` the whole time, and
//! prints the summary only when it changed.
//!
//! Every line a loop prints also goes to `.radian/arc/loop.log`, each prefixed with an RFC 3339
//! UTC timestamp, flushed per line ([`LoopLog`]). The arc TUI reads that file for its ticker
//! and its log view (ADR 0048 decision 13).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::store::attempt_ops::{self, Claim};
use crate::store::model::{self, TaskState};
use crate::store::{Error, PlanView, RUNNER_LOCK, Store, plan_ops};

/// A hard stop on the pass count. Every pass that continues changes at least one task; this
/// only keeps a store that something else keeps rewriting from looping forever.
pub const MAX_PASSES: u32 = 1000;

/// The default wait between passes of `serve`, and the closed range it may have.
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

/// Set by the SIGINT/SIGTERM handler a `serve` loop installs.
static STOP: AtomicBool = AtomicBool::new(false);

/// How many passes in a row may return the same error text before `serve` stops. A constant,
/// not a configuration key: a permanent error (a `config.toml` that cannot be read, a
/// directory that cannot be written) would otherwise print the same line unattended all
/// night. [`repeated_error_stop`] spells the line from it; the user manual and ADR 0048 quote
/// the value, so change them with it.
pub const REPEATED_ERROR_LIMIT: u32 = 3;

/// The one line a `serve` loop prints, with its `radian arc: ` prefix, before it replaces
/// itself with a rebuilt binary. The user manual quotes it.
pub const RESTART_TEXT: &str = "the radian binary changed on disk; restarting the loop with it";

/// How long a loop waits before its second look at a changed binary, so a build that is still
/// writing the file is not run (ADR 0048 decision 13).
const RECHECK: Duration = Duration::from_secs(1);

/// The paths whose change in a merged task means that main gained arc's own code (ADR 0048
/// decision 13). One list for the loop and the manual.
pub const ARC_CODE_PREFIXES: [&str; 3] = [
    "crates/radian-arc/",
    "crates/radian-tui/src/arc/",
    "crates/radian-tui/src/cli/arc",
];

/// The one line a scheduling pass prints, with its `radian arc: ` prefix, when a merge
/// changed arc's own code. The user manual quotes it.
#[must_use]
pub fn arc_code_changed(task: &str) -> String {
    format!(
        "{task} changed arc's code on main; rebuild radian (the loop restarts itself when its \
         binary changes)"
    )
}

/// The one line a loop's last pass reports instead of the repeated error; the CLI prints it
/// with its usual `radian arc: ` prefix and exits `1`. The count is [`REPEATED_ERROR_LIMIT`],
/// so the line cannot drift from the constant.
#[must_use]
pub fn repeated_error_stop() -> String {
    format!(
        "the same error {REPEATED_ERROR_LIMIT} passes in a row; stopping (fix it and start the \
         loop again)"
    )
}

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
/// **A rebuilt binary replaces the loop's code.** At start the loop records its executable's
/// path (once) and that file's identity; after each pass, before the interval, it compares the
/// file at that path with the recorded identity and, when a stable executable differs, prints
/// [`RESTART_TEXT`], releases `runner.lock` and `exec`s the recorded path with the same
/// arguments, working directory and environment (ADR 0048 decision 13). It stops no gate and
/// touches no worker. When the `exec` fails it prints the error, takes the lock again (exit
/// `1` when it cannot), records the new identity and goes on with the old code.
///
/// Three passes in a row that return the same error text stop the loop
/// ([`REPEATED_ERROR_LIMIT`]): the loop reports [`repeated_error_stop`] instead of the third
/// error, stops the gates the way a signal does (workers are left alone) and returns an error,
/// so the caller exits `1`. A pass that succeeds, or a different error, starts the count again.
///
/// # Errors
///
/// The same as [`run`], and [`repeated_error_stop`] after three identical pass errors.
pub fn serve(store: &Store, interval_secs: u64, emit: &mut dyn FnMut(&str)) -> Result<(), Error> {
    let mut log = LoopLog::new(store);
    let mut lock = take_lock(store, &mut log)?;
    STOP.store(false, Ordering::SeqCst);
    catch_stop_signals();
    // The executable this loop runs, read once: after a replace, `current_exe()` names a
    // deleted file on Linux (ADR 0048 decision 13).
    let executable = std::env::current_exe().ok();
    let mut recorded = executable.as_deref().and_then(executable_identity);
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut last: Option<String> = None;
    // The error text of the pass before and how many passes in a row returned it.
    let mut repeated: Option<(String, u32)> = None;
    loop {
        if stop_requested() {
            break;
        }
        let result = {
            let mut sink = |text: &str| say(&mut log, emit, text);
            run_quiet(store, &mut sink, Some(&STOP))
        };
        match result {
            Ok(report) => {
                // A pass that succeeds starts the repeated-error count again.
                repeated = None;
                let summary = report.render_summary();
                if last.as_deref() != Some(summary.as_str()) {
                    say(&mut log, emit, &summary);
                    last = Some(summary);
                }
            }
            // One task's error does not end the loop: print it and run the next pass. The
            // lock and every state file stay as they are; a one-shot `run` still returns
            // the error and exits non-zero. A permanent error would repeat forever, so the
            // third pass in a row with the same text stops the loop instead (the lock is
            // released when this returns, and the gates are stopped as on a signal).
            Err(error) => {
                let text = error.message();
                let count = match &repeated {
                    Some((last_text, count)) if *last_text == text => count + 1,
                    _ => 1,
                };
                if count >= REPEATED_ERROR_LIMIT {
                    stop_gates(store);
                    // The stop line is a line this loop prints, so it goes to `loop.log` like
                    // every other `radian arc:` line; the caller prints it once more on stderr.
                    say(
                        &mut log,
                        emit,
                        &format!("radian arc: {}\n", repeated_error_stop()),
                    );
                    return Err(Error::Failed(repeated_error_stop()));
                }
                say(&mut log, emit, &format!("radian arc: {text}\n"));
                repeated = Some((text, count));
            }
        }
        if stop_requested() {
            break;
        }
        // A rebuilt binary: one line, the lock released, and `exec` of the recorded path. A
        // gate and a worker are separate process groups and are left alone (ADR 0048
        // decision 13).
        if let (Some(path), Some(identity)) = (executable.as_deref(), recorded) {
            let mut look = |path: &Path| executable_identity(path);
            let mut pause = |wait: Duration| std::thread::sleep(wait);
            if let Some(new_identity) = changed_binary(path, identity, &mut look, &mut pause) {
                if stop_requested() {
                    break;
                }
                say(&mut log, emit, &format!("radian arc: {RESTART_TEXT}\n"));
                drop(lock);
                let exec_error = replace_process(path, &arguments);
                lock = take_lock_after_failed_exec(store, &mut log, emit, &exec_error)?;
                recorded = Some(new_identity);
            }
        }
        if !wait_interval(interval_secs) {
            break;
        }
    }
    if stop_requested() {
        stop_gates(store);
    }
    drop(lock);
    Ok(())
}

/// Writes a loop line through the log and the caller's sink. A transition line is also kept in
/// the [`Report`]; a `radian arc:` notice is not.
fn say(log: &mut LoopLog, emit: &mut dyn FnMut(&str), text: &str) {
    log.write(text);
    emit(text);
}

/// One identity of the running binary's file: what the loop compares after each pass (ADR 0048
/// decision 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BinaryIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified: Option<SystemTime>,
}

/// The identity of the file at `path` when it is a regular file with an execute bit. `None`
/// for a missing file, a directory, a special file, or a file a build is still writing (no
/// execute bit yet): the loop compares nothing then and looks again at a later pass.
fn executable_identity(path: &Path) -> Option<BinaryIdentity> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || !executable(&metadata) {
        return None;
    }
    Some(BinaryIdentity {
        device: device_of(&metadata),
        inode: inode_of(&metadata),
        size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(unix)]
fn executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.dev()
}

#[cfg(not(unix))]
fn device_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.ino()
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// The identity of a changed, stable executable at `path`: the first look differs from
/// `recorded`, so the function looks again after one second and returns the identity only when
/// the second look equals the first (ADR 0048 decision 13). `look` and `pause` are the seam
/// the unit tests use; production passes [`executable_identity`] and [`std::thread::sleep`].
fn changed_binary(
    path: &Path,
    recorded: BinaryIdentity,
    look: &mut dyn FnMut(&Path) -> Option<BinaryIdentity>,
    pause: &mut dyn FnMut(Duration),
) -> Option<BinaryIdentity> {
    let first = look(path)?;
    if first == recorded {
        return None;
    }
    pause(RECHECK);
    let second = look(path)?;
    (second == first).then_some(second)
}

/// Prints the failed `exec` and takes `runner.lock` again. When the lock cannot be taken —
/// another process took it in the gap, or the file cannot be opened — the reason the loop stops
/// goes through the loop's logger too, so `loop.log` holds every `radian arc:` line `serve`
/// prints (ADR 0048 decision 13); the caller then exits `1` with the same text.
fn take_lock_after_failed_exec(
    store: &Store,
    log: &mut LoopLog,
    emit: &mut dyn FnMut(&str),
    exec_error: &std::io::Error,
) -> Result<crate::store::RunnerLock, Error> {
    say(
        log,
        emit,
        &format!("radian arc: cannot restart with the new binary: {exec_error}\n"),
    );
    let text = match store.try_runner_lock() {
        Ok(Some(retaken)) => return Ok(retaken),
        Ok(None) => format!(
            "the new binary could not start ({exec_error}); {RUNNER_LOCK} is held by another \
             process"
        ),
        Err(error) => format!(
            "the new binary could not start ({exec_error}); did not take {RUNNER_LOCK} again: {}",
            error.message()
        ),
    };
    say(log, emit, &format!("radian arc: {text}\n"));
    Err(Error::Failed(text))
}

/// Replaces this process with `program` and its arguments, in the same working directory and
/// environment. It returns [`std::process::CommandExt::exec`]'s error; on success it never
/// returns (ADR 0048 decision 13).
#[cfg(unix)]
fn replace_process(program: &Path, arguments: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    std::process::Command::new(program).args(arguments).exec()
}

#[cfg(not(unix))]
fn replace_process(_program: &Path, _arguments: &[OsString]) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, "exec is not available")
}

/// Stops the gates a serve loop leaves running: a short grace, then SIGKILL. Each gate arc
/// stopped records `interrupted`, so the next loop starts it again and counts no death.
///
/// Best effort: the repeated-error stop may be running because `config.toml` itself cannot be
/// read, so an unreadable config falls back to the default grace and an unreadable task list
/// stops nothing. The caller then reports the stop line, not the config error.
fn stop_gates(store: &Store) {
    let grace = Duration::from_secs(
        store
            .config()
            .map(|config| config.limits.cancel_grace_secs)
            .unwrap_or_else(|_| crate::store::config::Limits::default().cancel_grace_secs),
    );
    let Ok(ids) = store.task_ids() else {
        return;
    };
    for id in ids {
        crate::gate::background::stop(store, &id, grace);
    }
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

    /// Writes a line the loop prints that is not a transition, so it reaches `loop.log` and
    /// the caller but is not part of the summary (the `changed arc's code` line).
    fn say(&mut self, line: String) {
        (self.emit)(&format!("{line}\n"));
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
/// (the `serve` signal flag) keeps the pass from starting an attempt or a merge once it is
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
                        // an automatic rework: it does not pause the plan. Neither does a
                        // failed reverify attempt, whether the loop or the owner started it.
                        let paused = match &verified.paused {
                            Some(paused) => Some(paused.clone()),
                            None if verified.phase == "rework_pending"
                                && verified.conflict.is_none() =>
                            {
                                if attempt_ops::is_reverify_attempt(store, task_id)? {
                                    None
                                } else {
                                    attempt_ops::record_auto_rework(store, task_id)?
                                }
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

/// Whether a path a merged task changed touches arc's own code (ADR 0048 decision 13): any
/// entry of [`ARC_CODE_PREFIXES`] is a prefix of it.
fn is_arc_code(path: &str) -> bool {
    ARC_CODE_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
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
pub(crate) fn task_claim_order(store: &Store) -> Result<Vec<String>, Error> {
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

/// Whether the task's plan is `cancelled`, by its `status` or by its tasks: the loop neither
/// claims nor gates its tasks (ADR 0048 decisions 3 and 13).
fn plan_is_cancelled(store: &Store, plan: &str) -> Result<bool, Error> {
    store.plan_is_cancelled(plan)
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
            match attempt_ops::claim_by(store, &task.item.id, attempt_ops::ClaimBy::SchedulingLoop)
            {
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
    // child so a long `serve` collects no zombie; the thread dies with this process, and
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
                    let crate::review::MergeStep {
                        line,
                        note,
                        changed_paths,
                    } = step;
                    if let Some(line) = line {
                        transitions.push(line);
                        changed = true;
                    }
                    if let Some(note) = note {
                        transitions.note(note);
                    }
                    // A merge that put arc's own code on main tells the person to rebuild;
                    // the loop then replaces itself when the binary changes (ADR 0048
                    // decision 13).
                    if changed_paths.iter().any(|path| is_arc_code(path)) {
                        transitions.say(format!("radian arc: {}", arc_code_changed(&task.item.id)));
                    }
                    let after = store.load_task(&task.item.id)?.item;
                    // A failed integration of a worker attempt counts; the same failure of a
                    // reverify attempt does not (ADR 0048 decisions 9 and 12).
                    let reverify = attempt_ops::is_reverify_attempt(store, &task.item.id)?;
                    if after.state == TaskState::Review
                        && after.phase == "rework_pending"
                        && !reverify
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
        if loaded.changed_outside_arc || !loaded.item.is_merged() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a scheduling pass would find a free concurrency slot right now: `confirm
/// --dry-run` says a ready task waits when it would have none.
///
/// A task file that does not read is not a running task here: a dry run must refuse exactly
/// as the real confirm does, and the real confirm never reads an unrelated task file.
pub(crate) fn free_concurrency_slot(store: &Store) -> Result<bool, Error> {
    let concurrency = u64::from(store.config()?.limits.concurrency.max(1));
    let mut doing = 0;
    for id in store.task_ids()? {
        if store
            .load_task(&id)
            .is_ok_and(|loaded| loaded.item.state == TaskState::Doing)
        {
            doing += 1;
        }
    }
    Ok(doing < concurrency)
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
        if view.is_cancelled() {
            continue;
        }
        let open: Vec<&crate::store::Loaded<model::TaskFile>> = view
            .tasks
            .iter()
            .filter(|task| !task.item.is_merged())
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
            let reason = waiting_reason(store, plan, &task.item)?;
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

/// Why `radian arc run` would not act on `task` now, in one line. `confirm --dry-run` uses
/// the same words for a task it would make ready.
pub fn waiting_reason(
    store: &Store,
    plan: &model::PlanFile,
    task: &model::TaskFile,
) -> Result<String, Error> {
    let item = task;
    let reason = match (item.state, item.phase.as_str()) {
        (TaskState::Todo, "ready") => {
            if plan.auto == "off" {
                "waiting for you to start it (`auto` is `off`)".to_owned()
            } else {
                let missing: Vec<&str> = item
                    .after
                    .iter()
                    .filter(|id| {
                        store
                            .load_task(id)
                            .is_ok_and(|loaded| !loaded.item.is_merged())
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

    fn identity(inode: u64) -> BinaryIdentity {
        BinaryIdentity {
            device: 1,
            inode,
            size: 10,
            modified: None,
        }
    }

    #[test]
    fn an_unchanged_binary_is_not_restarted() {
        let recorded = identity(1);
        let mut looks = std::collections::VecDeque::from([Some(recorded)]);
        let mut look = |_path: &Path| looks.pop_front().flatten();
        let mut pauses = 0;
        let mut pause = |_: Duration| pauses += 1;
        assert_eq!(
            changed_binary(Path::new("/bin/radian"), recorded, &mut look, &mut pause),
            None
        );
        assert_eq!(pauses, 0, "an unchanged file needs no second look");
    }

    #[test]
    fn a_changed_and_stable_binary_is_restarted() {
        let recorded = identity(1);
        let changed = identity(2);
        let mut looks = std::collections::VecDeque::from([Some(changed), Some(changed)]);
        let mut look = |_path: &Path| looks.pop_front().flatten();
        let mut pauses = Vec::new();
        let mut pause = |wait: Duration| pauses.push(wait);
        assert_eq!(
            changed_binary(Path::new("/bin/radian"), recorded, &mut look, &mut pause),
            Some(changed)
        );
        assert_eq!(pauses, vec![RECHECK], "the second look waits one second");
    }

    #[test]
    fn a_missing_binary_is_not_restarted() {
        let recorded = identity(1);
        let mut looks = std::collections::VecDeque::from([None]);
        let mut look = |_path: &Path| looks.pop_front().flatten();
        let mut pauses = 0;
        let mut pause = |_: Duration| pauses += 1;
        assert_eq!(
            changed_binary(Path::new("/bin/radian"), recorded, &mut look, &mut pause),
            None
        );
        assert_eq!(pauses, 0);
    }

    /// A build still writing the file changes it between the two looks: the loop waits for a
    /// later pass instead of running a half-written binary. Mutation: return the first look
    /// and this test runs it.
    #[test]
    fn a_binary_that_changes_between_the_two_looks_is_not_restarted() {
        let recorded = identity(1);
        let mut looks = std::collections::VecDeque::from([Some(identity(2)), Some(identity(3))]);
        let mut look = |_path: &Path| looks.pop_front().flatten();
        let mut pauses = 0;
        let mut pause = |_: Duration| pauses += 1;
        assert_eq!(
            changed_binary(Path::new("/bin/radian"), recorded, &mut look, &mut pause),
            None
        );
        assert_eq!(pauses, 1, "the second look ran");
    }

    /// The real metadata check: a missing file, a regular file without an execute bit and a
    /// directory have no identity, so the loop ignores a build that is still writing.
    #[cfg(unix)]
    #[test]
    fn a_missing_or_non_executable_file_has_no_identity() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_support::TempDir::new("binary-identity");
        assert_eq!(executable_identity(&dir.path().join("missing")), None);
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"not a binary").expect("write the plain file");
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644))
            .expect("remove the execute bits");
        assert_eq!(executable_identity(&plain), None);
        let program = dir.path().join("program");
        std::fs::write(&program, b"#!/bin/sh\n").expect("write the program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        assert!(executable_identity(&program).is_some());
        assert_eq!(
            executable_identity(dir.path()),
            None,
            "a directory is not a binary"
        );
    }

    /// A restart whose `exec` failed and whose lock another process took in the gap stops the
    /// loop with a line that also reaches `loop.log`: the log holds every `radian arc:` line
    /// `serve` prints (ADR 0048 decision 13). Mutation: return the error without `say` and the
    /// log and the sink keep only the `cannot restart` line.
    #[test]
    fn a_failed_restart_that_cannot_retake_the_lock_logs_the_reason_before_it_stops() {
        let dir = crate::test_support::TempDir::new("restart-lock-held");
        let store = Store::at(dir.path());
        std::fs::create_dir_all(store.root()).expect("the arc directory");
        let mut log = LoopLog::new(&store);
        // A second open file description of the same lock file: `try_runner_lock` sees it as
        // another process's lock, exactly as a loop started in the gap would.
        let held = store
            .try_runner_lock()
            .expect("take runner.lock")
            .expect("runner.lock is free");
        let mut emitted = String::new();
        let mut emit = |text: &str| emitted.push_str(text);
        let exec_error = std::io::Error::from_raw_os_error(22);

        let error = take_lock_after_failed_exec(&store, &mut log, &mut emit, &exec_error)
            .expect_err("another process holds runner.lock");
        drop(held);

        let expected = format!(
            "the new binary could not start ({exec_error}); {RUNNER_LOCK} is held by another \
             process"
        );
        assert_eq!(error.message(), expected);
        let said = format!(
            "radian arc: cannot restart with the new binary: {exec_error}\nradian arc: {expected}\n"
        );
        assert_eq!(emitted, said, "the sink saw the stop line");
        let written = std::fs::read_to_string(log.path()).expect("loop.log");
        assert!(
            written.contains(&format!("radian arc: {expected}")),
            "loop.log is missing the stop line: {written}"
        );
    }

    /// The same failure when `runner.lock` cannot be opened at all: the stop line is logged too.
    #[test]
    fn a_failed_restart_that_cannot_open_the_lock_logs_the_reason_before_it_stops() {
        let dir = crate::test_support::TempDir::new("restart-lock-unopenable");
        let store = Store::at(dir.path());
        // A directory in the lock's place makes the lock file impossible to open.
        std::fs::create_dir_all(store.root().join(RUNNER_LOCK)).expect("a directory at the lock");
        let mut log = LoopLog::new(&store);
        let mut emitted = String::new();
        let mut emit = |text: &str| emitted.push_str(text);
        let exec_error = std::io::Error::from_raw_os_error(22);

        let error = take_lock_after_failed_exec(&store, &mut log, &mut emit, &exec_error)
            .expect_err("the lock cannot be opened");

        assert!(
            error
                .message()
                .contains(&format!("did not take {RUNNER_LOCK} again")),
            "{}",
            error.message()
        );
        let line = format!("radian arc: {}", error.message());
        assert!(
            emitted.ends_with(&format!("{line}\n")),
            "the sink: {emitted}"
        );
        let written = std::fs::read_to_string(log.path()).expect("loop.log");
        assert!(
            written.contains(&line),
            "loop.log is missing the stop line: {written}"
        );
    }

    /// The path list has exactly the three entries ADR 0048 decision 13 and the manual name.
    /// Mutation: drop `crates/radian-tui/src/arc/` and the second assertion fails.
    #[test]
    fn arc_code_is_the_three_path_prefixes() {
        for path in [
            "crates/radian-arc/src/run.rs",
            "crates/radian-tui/src/arc/render.rs",
            "crates/radian-tui/src/cli/arc.rs",
        ] {
            assert!(is_arc_code(path), "{path} is arc code");
        }
        for path in [
            "crates/radian-tui/src/cli/attach.rs",
            "crates/radian-tui/src/arcade/x.rs",
            "docs/adr/0048-arc-orchestrator.md",
        ] {
            assert!(!is_arc_code(path), "{path} is not arc code");
        }
    }
}
