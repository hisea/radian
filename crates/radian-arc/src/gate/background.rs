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
//! the verdicts are all there.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{Value, json};

use crate::store::{Error, Store, model};

use super::stage;

/// The record a background gate leaves for the next pass.
pub const GATE_FILE: &str = "gate.json";
/// The background gate process's output, next to the record.
pub const GATE_LOG: &str = "gate.log";
/// How often [`stop`] looks at a signalled gate.
const STOP_POLL: Duration = Duration::from_millis(100);

/// One gate's outcome, as a pass reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `verify` or `integration`.
    pub kind: String,
    /// The attempt id the gate wrote its verdicts under.
    pub attempt: String,
    /// `running`, `done`, `refused` or `error`.
    pub status: String,
    /// The refusal or error text.
    pub text: Option<String>,
    /// The gate process, while it is `running`.
    pub pid: Option<u32>,
    /// The gate process's `ps` start time, while it is `running`.
    pub process_started_at: Option<String>,
    /// The digest of the inputs the gate saw (`plan_verification`'s `inputs`), when it could
    /// plan. A pass uses it to skip an unchanged refusal.
    pub inputs: Option<String>,
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

/// The task's gate record, when one was written.
#[must_use]
pub fn read(store: &Store, task: &str) -> Option<Record> {
    let text = fs::read_to_string(record_path(store, task)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(Record {
        kind: value.get("kind")?.as_str()?.to_owned(),
        attempt: value.get("attempt")?.as_str()?.to_owned(),
        status: value.get("status")?.as_str()?.to_owned(),
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
    })
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

/// Starts one background gate for `task`, unless one is already running. Returns the child's
/// pid when it started one.
///
/// The pass locks `gates/<task>.gate.lock` and clears the descriptor's close-on-exec flag, so
/// the child inherits the lock. There is no window in which a second pass could start a
/// second gate for the same task: the lock is either held by the child or free because the
/// child is gone.
///
/// # Errors
///
/// The lock file, the gate's log, or the child process could not be created.
pub fn start(store: &Store, task: &str) -> Result<Option<u32>, Error> {
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
    let mut child = command
        .spawn()
        .map_err(|error| Error::Failed(format!("cannot start `radian arc gate`: {error}")))?;
    let pid = child.id();
    // The parent's copy goes now; the child's keeps the lock. A thread waits for the child so
    // a long `--watch` run does not collect zombies; the thread dies with this process, and
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

/// Signals a running gate and gives it `grace` before SIGKILL. A `--watch` loop calls this
/// after it stops: the gate never finishes, so the next loop starts it again as unverified.
pub fn stop(store: &Store, task: &str, grace: Duration) {
    let Some(record) = read(store, task) else {
        return;
    };
    if record.status != "running" {
        return;
    }
    let Some(pid) = record.pid else {
        return;
    };
    // Only the process the record names: a reused pid has another start time. Without a
    // recorded start time (the sandbox refused `ps`) the pid is all there is.
    let started = record.process_started_at.unwrap_or_default();
    let alive = || started.is_empty() || crate::process::same_process(pid, &started);
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
    }
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
    // A `--watch` stop reaches this process; its stage child must get it too, or the child
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
    // While the gate runs, the record names this process, so a stop can signal it.
    let running = json!({
        "kind": kind,
        "attempt": label,
        "status": "running",
        "pid": std::process::id(),
        "process_started_at": crate::process::start_time(std::process::id()),
        "started_at": crate::store::plan_ops::utc_now(),
    });
    write(&store, &task.id, &running)
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}")))?;

    let result = if kind == "verify" {
        match crate::review::plan_verification(&store, &task.id) {
            Ok(plan) => {
                inputs = plan.inputs.clone();
                crate::review::run_stages(&store, &plan, None).map(|_| plan.label.clone())
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
        Ok(attempt) => write(
            &store,
            &task.id,
            &json!({
                "kind": kind,
                "attempt": attempt,
                "status": "done",
                "inputs": inputs,
            }),
        )
        .map_err(|error| Error::Failed(format!("cannot write the gate record: {error}"))),
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
