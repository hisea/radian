//! A gate in the background (ADR 0048 decisions 10 and 13).
//!
//! A scheduling pass starts a gate and returns: the gate is this binary's hidden
//! `radian arc gate` command, one process per task. It runs the stages (or the integration
//! stage), writes the verdicts and one record ([`read`]), and exits. Only a pass that holds
//! `runner.lock` moves a task's state, from the verdicts and the record the gate left.
//!
//! One gate runs per task at a time. [`start`] opens `gates/<task>.gate.lock`, locks it and
//! gives the child the locked descriptor: the child inherits the lock, so a second pass finds
//! it held and starts nothing. A gate that dies closes its descriptor; the next pass starts
//! the gate again, and [`crate::review::run_stages`] reuses the verdicts already written, so
//! the stages resume rather than rerun.
//!
//! The record is `runs/<task>/gate.json`. The child writes `running` with its own pid as soon
//! as it starts, and the outcome last, after its verdicts, so a record that says `done` means
//! the verdicts are all there. A gate the loop itself stopped is `interrupted`; every start
//! whose gate is gone without a `done`, `refused` or `error` record is counted, once, and
//! `[limits] max_gate_deaths` turns it into `died` (ADR 0048 decision 10). A `started` record
//! that kept the digest a pass planned is the one exception: its gate left no record, so the
//! pass reports `no_record` and starts no gate for those inputs.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gate::config::{Profile, StageSpec};
use crate::store::{Error, Store, model};

use super::stage;

/// The record a background gate leaves for the next pass.
pub const GATE_FILE: &str = "gate.json";
/// The background gate process's output, next to the record.
pub const GATE_LOG: &str = "gate.log";
/// A record status: a pass wrote the record it started a gate with (and the digest of the
/// inputs, when it could plan them) and the gate process has not written its own record yet.
pub const STARTED: &str = "started";
/// A record status: the gate process is running, with its pid and process start time.
pub const RUNNING: &str = "running";
/// A record status: the gate finished and its verdicts are all there.
pub const DONE: &str = "done";
/// A record status: a check before the stages refused the gate.
pub const REFUSED: &str = "refused";
/// A record status: the gate failed.
pub const ERROR: &str = "error";
/// A record status: a gate process exited without writing any record at all (a panic before
/// its first write, a store that could not be opened).
pub const NO_RECORD: &str = "no_record";
/// A record status: a gate process wrote its `running` record and then died, more often than
/// `[limits] max_gate_deaths` allows for the same attempt and inputs.
pub const DIED: &str = "died";
/// A record status: arc stopped the gate itself (a stop signal on the scheduling loop), so the
/// next pass starts it again without counting a death.
pub const INTERRUPTED: &str = "interrupted";
/// The text a `no_record` gate is reported with: `radian arc run` prints it after the task id
/// and `show` prints it as the `rework_reason`.
pub const NO_RECORD_TEXT: &str = "its gate exited without a record";
/// The text a `died` gate is reported with, the same two places.
pub const DIED_TEXT: &str = "its gate died after it started";
/// The text an `interrupted` record keeps.
pub const INTERRUPTED_TEXT: &str = "arc stopped the gate";
/// Every status a gate record can have, in one list: [`read`] refuses anything else, and the
/// manual's drift test reads it (ADR 0048 decision 10).
pub const STATUSES: &[&str] = &[
    STARTED,
    RUNNING,
    DONE,
    REFUSED,
    ERROR,
    NO_RECORD,
    DIED,
    INTERRUPTED,
];
/// Test-only (an input that exists only for tests is not in the manual): makes the gate
/// process exit before it writes any record, the way a gate that panics before its first
/// write does. `run.rs` sets it on the `radian arc run` process, which the gate child
/// inherits. Never set by arc.
pub const EXIT_WITHOUT_RECORD_ENV: &str = "RADIAN_ARC_GATE_TEST_EXIT_WITHOUT_RECORD";
/// Test-only: makes the gate process exit right after it wrote its `running` record, the way
/// a gate that crashes in a stage does. The record keeps `running`, so the next pass counts a
/// death. Never set by arc.
pub const DIE_AFTER_RUNNING_ENV: &str = "RADIAN_ARC_GATE_TEST_DIE_AFTER_RUNNING";
/// Test-only: how long the gate process waits before its `running` record, so a test can
/// signal the loop while only the pass's `started` record exists. Milliseconds. Never set by
/// arc.
pub const SLOW_START_ENV: &str = "RADIAN_ARC_GATE_TEST_SLOW_START_MS";
/// How often [`stop`] looks at a signalled gate.
const STOP_POLL: Duration = Duration::from_millis(100);
/// How long [`stop`] waits for a gate that was started just before the signal to name its
/// process: the pass writes the pid right after the spawn, and the gate writes `running` a
/// moment later.
const STOP_START_TIMEOUT: Duration = Duration::from_secs(2);
/// How long [`stop`] waits, after SIGKILL, for the gate to be gone: the record says arc
/// stopped it only once the process is off the lock.
const STOP_KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// The gate profile a verification ran under, as the gate planned it. It is written into the
/// `done` record so the pass that applies the verdicts judges them by the profile that produced
/// them, even when `config.toml` changes while the gate runs (ADR 0048 decisions 10 and 15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedProfile {
    /// The `[gate.profiles.<name>]` name every verdict of the run records.
    pub name: String,
    /// The profile itself, as the gate read it.
    pub profile: Profile,
    /// The effective stages: the profile's plus the task's `extra_stages`, as the gate ran them.
    pub stages: Vec<StageSpec>,
}

/// One gate's outcome, as a pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `verify` or `integration`.
    pub kind: String,
    /// The attempt id the gate wrote its verdicts under.
    pub attempt: String,
    /// The record's status: one of [`STATUSES`].
    pub status: String,
    /// The refusal or error text, or the text a `no_record` or `died` record is reported with.
    pub text: Option<String>,
    /// The gate process, while the record is `running` or `started`.
    pub pid: Option<u32>,
    /// The gate process's `ps` start time, while it is `running` or `started`.
    pub process_started_at: Option<String>,
    /// The digest of the inputs the gate saw (`plan_verification`'s `inputs`), when it could
    /// plan. A pass uses it to skip an unchanged refusal.
    pub inputs: Option<String>,
    /// How many times a gate died after writing `running` for this attempt and these inputs
    /// (ADR 0048 decision 10). A pass compares it with `[limits] max_gate_deaths`.
    pub deaths: u32,
    /// The profile a `done` verification planned, when the record carries one: a pass applies
    /// the verdicts with it instead of re-reading the flow's profile from a `config.toml` that
    /// may have changed since the gate started (ADR 0048 decision 10).
    pub profile: Option<PlannedProfile>,
}

/// The record path for one task.
#[must_use]
pub fn record_path(store: &Store, task: &str) -> PathBuf {
    store.root().join("runs").join(task).join(GATE_FILE)
}

/// The lock path one task's running gate holds.
fn lock_path(repository: &Path, task: &str) -> PathBuf {
    crate::gate::gates_dir(repository).join(format!("{task}.gate.lock"))
}

/// The task's gate record, when one was written. A record whose status is not in [`STATUSES`]
/// is not an arc record: it reads as none, and the next pass starts the gate again.
#[must_use]
pub fn read(store: &Store, task: &str) -> Option<Record> {
    let value = read_value(store, task)?;
    let status = value.get("status")?.as_str()?;
    if !STATUSES.contains(&status) {
        return None;
    }
    Some(Record {
        kind: value.get("kind")?.as_str()?.to_owned(),
        attempt: value.get("attempt")?.as_str()?.to_owned(),
        status: status.to_owned(),
        text: value.get("text").and_then(Value::as_str).map(str::to_owned),
        pid: value
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
        process_started_at: value
            .get("process_started_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        inputs: value
            .get("inputs")
            .and_then(Value::as_str)
            .filter(|inputs| !inputs.is_empty())
            .map(str::to_owned),
        deaths: value
            .get("deaths")
            .and_then(Value::as_u64)
            .and_then(|deaths| u32::try_from(deaths).ok())
            .unwrap_or(0),
        profile: value
            .get("profile")
            .and_then(|profile| serde_json::from_value(profile.clone()).ok()),
    })
}

/// The record file as it was written, before any field is interpreted.
fn read_value(store: &Store, task: &str) -> Option<Value> {
    let text = fs::read_to_string(record_path(store, task)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Removes the record once a pass has applied it. A record left behind is harmless (the state
/// moved away from the gate's phase), but the next gate overwrites it anyway.
pub fn clear(store: &Store, task: &str) {
    let _ = fs::remove_file(record_path(store, task));
}

fn write(store: &Store, task: &str, value: &Value) -> io::Result<()> {
    let path = record_path(store, task);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&temporary, format!("{text}\n"))?;
    fs::rename(&temporary, &path)
}

/// Records that a gate a pass started exited without writing any record: the same record and
/// `inputs` field a `refused` gate leaves, so a later pass compares the inputs the same way
/// and starts no gate for the same ones (ADR 0048 decision 10).
///
/// # Errors
///
/// The record could not be written.
pub fn write_no_record(
    store: &Store,
    task: &str,
    kind: &str,
    attempt: &str,
    inputs: &str,
) -> Result<(), Error> {
    write(
        store,
        task,
        &json!({
            "kind": kind,
            "attempt": attempt,
            "status": NO_RECORD,
            "text": NO_RECORD_TEXT,
            "inputs": inputs,
            "at": crate::store::plan_ops::utc_now(),
        }),
    )
    .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))
}

/// Records that a gate died once too often for the same attempt and the same inputs: the
/// status [`DIED`] behaves exactly as [`NO_RECORD`] does, so a later pass compares the inputs
/// the same way and starts no gate for the same ones (ADR 0048 decision 10). `inputs` is
/// `None` when the pass could not plan the gate: the record then carries no digest, and a
/// later pass that can plan once more starts one new gate.
///
/// # Errors
///
/// The record could not be written.
pub fn write_died(
    store: &Store,
    task: &str,
    kind: &str,
    attempt: &str,
    inputs: Option<&str>,
    deaths: u32,
) -> Result<(), Error> {
    let mut value = json!({
        "kind": kind,
        "attempt": attempt,
        "status": DIED,
        "text": DIED_TEXT,
        "deaths": deaths,
        "at": crate::store::plan_ops::utc_now(),
    });
    if let Some(inputs) = inputs {
        value["inputs"] = json!(inputs);
    }
    write(store, task, &value)
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))
}

/// What a pass does about a gate start whose gate is gone (ADR 0048 decision 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Death {
    /// The gate died; it is started again with this many deaths in its record.
    Count(u32),
    /// It died once too often for these inputs: the record is `died` and nothing starts.
    Stop,
}

/// Counts one death of a start whose gate is gone: its record is `running`, or `started` with
/// no digest. `inputs` is the digest of what the gate would read now: a different digest on
/// both sides begins the count again at 1, and an unknown one on either side (the gate could
/// not be planned then, or cannot be planned now) carries the count forward, because there is
/// nothing to compare. Also writes the `died` record when the count passes
/// `[limits] max_gate_deaths`; it keeps the digest only when the pass has one now.
///
/// A start is counted exactly once: every start whose gate is gone without a `done`, `refused`
/// or `error` record is one death, so `1 + max_gate_deaths` bounds the starts of one attempt
/// even when a gate could not be planned.
///
/// # Errors
///
/// The config or the record could not be read or written.
pub fn count_death(
    store: &Store,
    task: &str,
    kind: &str,
    attempt: &str,
    record: &Record,
    inputs: Option<&str>,
) -> Result<Death, Error> {
    let deaths = match (record.inputs.as_deref(), inputs) {
        (Some(before), Some(now)) if before != now => 1,
        _ => record.deaths.saturating_add(1),
    };
    if deaths > store.config()?.limits.max_gate_deaths {
        write_died(store, task, kind, attempt, inputs, deaths)?;
        return Ok(Death::Stop);
    }
    Ok(Death::Count(deaths))
}

/// Whether the task's gate is stuck: its record names the task's current attempt with status
/// `no_record` or `died`, and no gate process holds the task's gate lock. `radian arc
/// reverify` accepts a `review.verifying` task only then (ADR 0048 decision 11).
#[must_use]
pub fn stuck(store: &Store, task: &str, attempt: &str) -> bool {
    if running(store, task) {
        return false;
    }
    read(store, task).is_some_and(|record| {
        record.attempt == attempt && matches!(record.status.as_str(), NO_RECORD | DIED)
    })
}

/// Whether a gate is running for this task. It looks at the lock only, never at a process:
/// the `workspace` sandbox refuses `ps`.
#[must_use]
pub fn running(store: &Store, task: &str) -> bool {
    let path = lock_path(store.repo(), task);
    let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
        return false;
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            false
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

/// Waits for the task's running gate, if any, and holds the task's gate lock for the caller.
/// `radian arc start` and `radian arc reverify` run the stages in their own process and use
/// this, so a background gate and a synchronous verification never run at once; the file is
/// the guard (dropping it releases the lock).
///
/// # Errors
///
/// The lock file or its directory could not be opened.
pub fn lock_task(store: &Store, task: &str) -> Result<File, Error> {
    let path = lock_path(store.repo(), task);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::Failed(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", path.display())))?;
    file.lock_exclusive()
        .map_err(|error| Error::Failed(format!("cannot lock {}: {error}", path.display())))?;
    Ok(file)
}

/// What a pass starts: the kind of gate (`verify` or `integration`), the attempt it names
/// its verdicts by, the digest of the inputs the gate will read, and how many times a gate
/// already died for them (ADR 0048 decision 10).
///
/// `inputs` is `None` when the pass could not plan the gate: the gate then writes its own
/// refusal or error record, and a gate that exits without one is started again (there is no
/// digest to compare).
pub struct Start {
    pub kind: &'static str,
    pub attempt: String,
    pub inputs: Option<String>,
    /// The deaths this attempt and these inputs have behind them; the next death is one more.
    pub deaths: u32,
}

/// Starts one background gate for `task`, unless one is already running. Returns the child's
/// pid when it started one.
///
/// The pass locks `gates/<task>.gate.lock` and clears the descriptor's close-on-exec flag, so
/// the child inherits the lock. There is no window in which a second pass could start a
/// second gate for the same task: the lock is either held by the child or free because the
/// child is gone.
///
/// Before it spawns, the pass writes the inputs it started the gate with (when it could plan
/// them) and the deaths it already has to the gate record ([`STARTED`]); the gate replaces the
/// status with `running` as its first write and keeps both fields. The record a gate process
/// that exits without writing anything leaves is what lets a later pass report it instead of
/// starting it again.
/// Once the child exists the pass names it in that record too, so a stop that finds only the
/// `started` record has a process to signal.
///
/// # Errors
///
/// The lock file, the gate's log, the pass's own record, or the child process could not be
/// created.
pub fn start(store: &Store, task: &str, start: &Start) -> Result<Option<u32>, Error> {
    let path = lock_path(store.repo(), task);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Error::Failed(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", path.display())))?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(error) => {
            return Err(Error::Failed(format!(
                "cannot lock {}: {error}",
                path.display()
            )));
        }
    }
    let runs = store.root().join("runs").join(task);
    fs::create_dir_all(&runs)
        .map_err(|error| Error::Failed(format!("cannot create {}: {error}", runs.display())))?;
    let log_path = runs.join(GATE_LOG);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", log_path.display())))?;
    let log_err = log
        .try_clone()
        .map_err(|error| Error::Failed(format!("cannot open {}: {error}", log_path.display())))?;

    // The inputs this pass starts the gate with, before the process exists: a gate that exits
    // without a record leaves the ones it was started with, and a gate that dies after
    // `running` leaves the count so far (ADR 0048 decision 10).
    let mut started = json!({
        "kind": start.kind,
        "attempt": start.attempt,
        "status": STARTED,
        "deaths": start.deaths,
        "started_at": crate::store::plan_ops::utc_now(),
    });
    if let Some(inputs) = &start.inputs {
        started["inputs"] = json!(inputs);
    }
    write(store, task, &started)
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))?;

    let fd = file.as_raw_fd();
    inherit(&file, &path)?;
    let executable = std::env::current_exe()
        .map_err(|error| Error::Failed(format!("cannot find the current executable: {error}")))?;
    let mut command = Command::new(executable);
    command
        .arg("arc")
        .arg("gate")
        .arg("--task")
        .arg(task)
        .arg("--lock-fd")
        .arg(fd.to_string())
        .current_dir(store.repo())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group: a terminal Ctrl-C reaches the scheduling loop only, which
        // then stops the gate through `stop` so the gate can stop its stage child first.
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        // The gate never ran, so the pass's record would make the next pass report a
        // no-record gate that never started.
        clear(store, task);
        Error::Failed(format!("cannot start `radian arc gate`: {error}"))
    })?;
    let pid = child.id();
    remember_pid(store, task, pid);
    // The parent's copy goes now; the child's keeps the lock. A thread waits for the child so
    // a long `serve` run does not collect zombies; the thread dies with this process, and
    // the child keeps running either way.
    drop(file);
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(Some(pid))
}

/// Clears `FD_CLOEXEC` on the locked descriptor, so the gate child inherits the lock.
fn inherit(file: &File, path: &Path) -> Result<(), Error> {
    let fd = file.as_raw_fd();
    // SAFETY: `fcntl` on a descriptor this process owns, with integer arguments and no
    // pointer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(Error::Failed(format!(
            "cannot read the descriptor of {}: {}",
            path.display(),
            io::Error::last_os_error()
        )));
    }
    // SAFETY: as above; `F_SETFD` sets a per-descriptor flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(Error::Failed(format!(
            "cannot inherit the lock on {}: {}",
            path.display(),
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Names the gate process in the pass's `started` record, unless the gate has already written
/// its own `running` record (which carries the same pid). A stop that arrives between the
/// spawn and that write finds the process here.
fn remember_pid(store: &Store, task: &str, pid: u32) {
    let Some(mut value) = read_value(store, task) else {
        return;
    };
    if value.get("status").and_then(Value::as_str) != Some(STARTED) {
        return;
    }
    value["pid"] = json!(pid);
    value["process_started_at"] = json!(crate::process::start_time(pid));
    let _ = write(store, task, &value);
}

/// Signals the task's gate and gives it `grace` before SIGKILL, then records that arc stopped
/// it (`interrupted`), so the next pass starts it again and counts no death (ADR 0048
/// decision 10). A `serve` loop calls this once it stops.
///
/// A gate is stopped when its record says `running`, and also when the record still says
/// `started` and the gate lock is held: a gate started just before the signal has not written
/// its `running` record yet, and its lock is the proof that the process is there. A gate the
/// record no longer names as running (it finished, or it exited on its own) is left alone.
pub fn stop(store: &Store, task: &str, grace: Duration) {
    let Some(record) = wait_for_gate(store, task) else {
        return;
    };
    let stopping = match record.status.as_str() {
        RUNNING => true,
        STARTED => running(store, task),
        _ => false,
    };
    if !stopping {
        return;
    }
    let Some(pid) = record.pid else {
        return;
    };
    // Only the process the record names: a reused pid has another start time. Without a
    // recorded start time, or when `ps` cannot report it (the sandbox refused `ps`), the pid
    // alone is what identity there is, and it is checked rather than assumed.
    let alive = || crate::process::still_the_process(pid, record.process_started_at.as_deref());
    if !alive() {
        return;
    }
    crate::process::signal(pid, crate::process::SIGTERM);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && alive() {
        std::thread::sleep(STOP_POLL);
    }
    if alive() {
        crate::process::signal(pid, crate::process::SIGKILL);
        // The record says arc stopped the gate only once the gate is gone: while it is alive
        // it still holds the lock, and the next loop would start nothing.
        let deadline = Instant::now() + STOP_KILL_TIMEOUT;
        while Instant::now() < deadline && alive() {
            std::thread::sleep(STOP_POLL);
        }
    }
    write_interrupted(store, task);
}

/// The task's record once a gate process is behind it: `running`, or `started` with the gate's
/// pid named. A gate that was started just before a stop signal gets a short while to write
/// one of the two.
fn wait_for_gate(store: &Store, task: &str) -> Option<Record> {
    let deadline = Instant::now() + STOP_START_TIMEOUT;
    loop {
        let record = read(store, task)?;
        if record.status == RUNNING
            || (record.status == STARTED && record.pid.is_some() && running(store, task))
        {
            return Some(record);
        }
        if record.status != STARTED || Instant::now() >= deadline {
            return Some(record);
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// Records that arc stopped the gate: `running` or `started` becomes `interrupted`, keeping
/// the kind, the attempt and the inputs. A record the gate finished in the meantime is left
/// as it is.
fn write_interrupted(store: &Store, task: &str) {
    let Some(record) = read(store, task) else {
        return;
    };
    if !matches!(record.status.as_str(), RUNNING | STARTED) {
        return;
    }
    let mut value = json!({
        "kind": record.kind,
        "attempt": record.attempt,
        "status": INTERRUPTED,
        "text": INTERRUPTED_TEXT,
        "at": crate::store::plan_ops::utc_now(),
    });
    if let Some(inputs) = record.inputs {
        value["inputs"] = json!(inputs);
    }
    let _ = write(store, task, &value);
}

// ---- the child side -------------------------------------------------------------------

/// The `radian arc gate` flags. Internal: a scheduling pass starts it, a person does not.
pub struct Options {
    /// The task whose gate to run.
    pub task: String,
    /// The inherited lock descriptor. The child owns it for its whole life, so the lock stays
    /// held from the moment [`start`] returned.
    pub lock_fd: Option<i32>,
}

/// Runs one background gate and returns the process exit code.
pub fn run(options: Options) -> i32 {
    // Test-only (see [`EXIT_WITHOUT_RECORD_ENV`]): a gate that exits before writing anything,
    // the way one that panics before its first write does.
    if std::env::var_os(EXIT_WITHOUT_RECORD_ENV).is_some() {
        eprintln!("radian arc gate: test: exiting without a record");
        return 1;
    }
    // Test-only (see [`SLOW_START_ENV`]): a gate that stays short of its `running` record, so
    // a test can stop the loop while only the pass's `started` record exists.
    if let Some(millis) = slow_start() {
        std::thread::sleep(Duration::from_millis(millis));
    }
    // Own the descriptor: taking it keeps the lock held until this process exits; closing it
    // earlier would let another pass start a second gate.
    let _lock = options.lock_fd.map(|fd| {
        // SAFETY: the descriptor is this process's own, inherited from the parent and named
        // by `--lock-fd`; `from_raw_fd` takes ownership of it exactly once.
        unsafe { File::from_raw_fd(fd) }
    });
    // The gate keeps the lock; its stage children must not inherit it. Without this, a
    // `commands` shell (or anything it leaves running) keeps `gates/<task>.gate.lock` after
    // the gate is gone, and the task wedges with no message.
    if let Some(lock) = &_lock
        && let Err(error) = cloexec(lock)
    {
        eprintln!("radian arc gate: {}", error.message());
        return 1;
    }
    // A `serve` stop reaches this process; its stage child must get it too, or the child
    // would keep writing in the checkout after the gate is gone.
    stage::install_handler();
    match run_inner(&options) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("radian arc gate: {}", error.message());
            1
        }
    }
}

/// Test-only (see [`SLOW_START_ENV`]): how long the gate waits before its `running` record.
fn slow_start() -> Option<u64> {
    std::env::var_os(SLOW_START_ENV)?
        .to_str()?
        .trim()
        .parse()
        .ok()
}

/// Sets `FD_CLOEXEC` on the inherited lock descriptor: the gate holds the lock, a stage child
/// does not.
fn cloexec(file: &File) -> Result<(), Error> {
    let fd = file.as_raw_fd();
    // SAFETY: `fcntl` on a descriptor this process owns, with integer arguments and no
    // pointer.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(Error::Failed(format!(
            "cannot read the gate lock descriptor: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: as above; `F_SETFD` sets a per-descriptor flag.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(Error::Failed(format!(
            "cannot close the gate lock on exec: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn run_inner(options: &Options) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|error| Error::Failed(format!("cannot read the current directory: {error}")))?;
    let store = Store::open(&cwd)?;
    stage::set_grace(Duration::from_secs(
        store.config()?.limits.cancel_grace_secs,
    ));
    let loaded = store.load_task(&options.task)?;
    if loaded.changed_outside_arc {
        return Ok(());
    }
    let task = loaded.item;
    let kind = match (task.state, task.phase.as_str()) {
        (model::TaskState::Review, "verifying") => "verify",
        (model::TaskState::Review, "integrating") => "integration",
        // The task moved since the pass started the gate: nothing to do, and nothing to
        // record. The pass that moved it already knows.
        _ => return Ok(()),
    };
    // The attempt is known before the stages run, so every record names it: a pass applies a
    // refused or error record only when it belongs to the task's current attempt.
    let attempt = crate::review::attempt_label(&task)?;
    let label = if kind == "verify" {
        attempt
    } else {
        crate::review::next_integration_label(&store, &task.id, &attempt)?
    };
    // Filled in when the verification can be planned; a planning refusal has none.
    let mut inputs = String::new();
    // The profile the stages run under, kept for the `done` record: a later pass applies the
    // verdicts with it even when `config.toml` changed after the gate started.
    let mut planned_profile: Option<PlannedProfile> = None;
    // While the gate runs, the record names this process, so a stop can signal it. The fields
    // the pass wrote are kept: the digest of the inputs it started the gate with and the
    // deaths the same attempt and inputs already have (ADR 0048 decision 10).
    let mut running = json!({
        "kind": kind,
        "attempt": label,
        "status": RUNNING,
        "pid": std::process::id(),
        "process_started_at": crate::process::start_time(std::process::id()),
        "started_at": crate::store::plan_ops::utc_now(),
    });
    if let Some(written) = read_value(&store, &task.id) {
        if let Some(written_inputs) = written
            .get("inputs")
            .and_then(Value::as_str)
            .filter(|inputs| !inputs.is_empty())
        {
            running["inputs"] = json!(written_inputs);
            // The pass computed an integration's digest; the integration stage has no plan to
            // re-derive it from, so its refusal, error and done records keep the digest the
            // gate was started with. A verification re-plans and replaces it below.
            if kind == "integration" {
                inputs = written_inputs.to_owned();
            }
        }
        if let Some(deaths) = written.get("deaths").and_then(Value::as_u64) {
            running["deaths"] = json!(deaths);
        }
    }
    write(&store, &task.id, &running)
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))?;
    // Test-only (see [`DIE_AFTER_RUNNING_ENV`]): a gate that wrote `running` and then died.
    if std::env::var_os(DIE_AFTER_RUNNING_ENV).is_some() {
        return Err(Error::Failed(
            "test: dying after the running record".to_owned(),
        ));
    }

    let result = if kind == "verify" {
        match crate::review::plan_verification(&store, &task.id) {
            Ok(plan) => {
                inputs = plan.inputs.clone();
                planned_profile = Some(PlannedProfile {
                    name: plan.profile_name.clone(),
                    profile: plan.profile.clone(),
                    stages: plan.stages.clone(),
                });
                // Test-only (see `crate::review::REVIEW_PROGRAM_ENV`): the fake review program a
                // test leaves for the gate, in place of `radian exec`.
                let review_program =
                    std::env::var_os(crate::review::REVIEW_PROGRAM_ENV).map(PathBuf::from);
                crate::review::run_stages(&store, &plan, review_program.as_deref())
                    .map(|_| plan.label.clone())
            }
            // A planning refusal has no inputs: the pass re-plans and, if it now succeeds,
            // knows the inputs changed.
            Err(error) => Err(error),
        }
    } else {
        crate::review::gate_integration(&store, &task.id)
    };
    // A stop is not an outcome: the stage was killed and the next loop starts it again as
    // unverified. Leave the `running` record and exit without a completion record.
    if stage::stop_requested() {
        return Err(Error::Failed("the gate was stopped".to_owned()));
    }
    match result {
        Ok(attempt) => {
            let mut done = json!({
                "kind": kind,
                "attempt": attempt,
                "status": "done",
                "inputs": inputs,
            });
            if let Some(profile) = &planned_profile {
                done["profile"] = json!(profile);
            }
            write(&store, &task.id, &done)
                .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))
        }
        Err(Error::Refused(text)) => write(
            &store,
            &task.id,
            &json!({
                "kind": kind,
                "attempt": label,
                "status": "refused",
                "text": text,
                "inputs": inputs,
            }),
        )
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}"))),
        Err(error) => {
            let _ = write(
                &store,
                &task.id,
                &json!({
                    "kind": kind,
                    "attempt": label,
                    "status": "error",
                    "text": error.message(),
                    "inputs": inputs,
                }),
            );
            Err(error)
        }
    }
}
