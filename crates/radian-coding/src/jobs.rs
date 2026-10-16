//! Background shell jobs (ADR 0037 slices 1-2).
//!
//! A background command runs under the **same sandbox and environment** as a foreground one
//! ([`crate::shell::sandboxed_command`]), in its own session and process group (`setsid`), but
//! the tool returns at once. Its output streams to `<id>.out` in the registry's jobs
//! directory (the full history) and into a bounded in-memory ring (`job_output`'s increments).
//! The registry is per session, owned by [`crate::provider::CodingSession`]; at most
//! [`MAX_RUNNING_JOBS`] run at once.
//!
//! The jobs directory is a **parameter**, never derived from the workspace (ADR 0039 decision
//! 1): the host passes the session's own directory (`<sessions_dir>/<session-id>.jobs/`,
//! 0700, on the sandbox's deny-read list), so a model that can write the workspace cannot
//! pre-plant a symlink the host would follow. The output file is opened
//! `O_CREAT|O_EXCL|O_NOFOLLOW` with mode 0600, and the model reads it only through
//! `job_output`; the fields that used to carry a path carry [`OUTPUT_HINT`] instead.
//!
//! The process-group invariant is the same one [`crate::shell`] uses: the leader is kept
//! **unreaped** while its group is reserved, and every signal is sent under the `reaped`
//! mutex, so a group id can never be reused under a kill. `job_kill` sends SIGTERM, then
//! SIGKILL after [`KILL_GRACE`]; a natural exit or a timeout kills the whole group the same
//! way. Killing never touches the `.out` file.
//!
//! Slice 3 will subscribe to settled jobs through [`Jobs::subscribe`]; this slice does not
//! wire the inbox.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agent_core::{
    CancellationToken, ContentPart, ToolConcurrency, ToolContext, ToolFailure, ToolOutput, ToolSpec,
};
use radian_sandbox::Sandbox;
use serde::Deserialize;
use serde_json::json;

use crate::args;
use crate::fail;
use crate::shell::{lock_bool, sandboxed_command, signal_group, wait_for_exit_without_reaping};

/// How many background jobs may run at once in one session (ADR 0037 decision 7). A job costs
/// a process group, a file and a ring; four is enough for a build, a test run and a server,
/// and keeps the cost bounded.
pub const MAX_RUNNING_JOBS: usize = 4;

/// The in-memory ring per job, in bytes, for `job_output` increments. 64 KiB is enough for the
/// interesting tail of a build log between two tool calls; four jobs hold at most 256 KiB. The
/// `.out` file keeps the full history for the host, but the model reads only through
/// `job_output`, so the ring is the read window and only has to cover one read gap.
pub const JOB_RING_BYTES: usize = 64 << 10;

/// SIGTERM, then SIGKILL after this grace (ADR 0037 decision 2).
const KILL_GRACE: Duration = Duration::from_secs(2);

/// `job_output`'s default wait, and its hard cap (dsh's numbers).
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

/// How long the supervisor waits for the output readers after the group is killed.
const READER_GRACE: Duration = Duration::from_secs(2);

/// What the model sees where the output file's path used to be (ADR 0039 decision 1).
///
/// The output file lives in the session directory, outside the workspace and on the sandbox's
/// deny-read list, so the model can neither open it nor be told a path it could use. Every
/// field that once carried a workspace-relative path (`StartInfo::output`,
/// `JobEvent::Started::output`, `JobSummary::output`, `OutputResult::output`) carries this
/// sentence instead, which names the one channel that still reaches the output.
pub const OUTPUT_HINT: &str = "available through job_output";

/// The `job_settled` broadcast capacity. A subscriber that falls this far behind loses old
/// notices; slice 3 re-reads the state instead of trusting the channel alone.
const SETTLED_CAPACITY: usize = 16;

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// One job's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Running,
    /// The leader exited with this code.
    Exited(i32),
    /// The leader was killed by this signal (not a requested kill).
    Signaled(i32),
    /// `job_kill` (or a session-end cleanup) stopped it.
    Killed,
    /// Its `timeout_secs` elapsed.
    TimedOut,
}

impl JobState {
    pub fn is_running(&self) -> bool {
        matches!(self, JobState::Running)
    }

    /// The `[status: ...]` spelling.
    pub fn label(&self) -> String {
        match self {
            JobState::Running => "running".to_owned(),
            JobState::Exited(0) => "exited 0".to_owned(),
            JobState::Exited(code) => format!("exited {code}"),
            JobState::Signaled(signal) => format!("signal {signal}"),
            JobState::Killed => "killed".to_owned(),
            JobState::TimedOut => "timed out".to_owned(),
        }
    }
}

/// Why a running job was stopped before its leader exited on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Termination {
    Killed,
    TimedOut,
}

/// One job's shared state, written by the supervisor and read by the tools.
struct JobCell {
    /// The command's first line (the summary the list shows).
    command: String,
    started: Instant,
    state: JobState,
    /// Set by `job_kill`/the timeout before the group is signalled, so the supervisor can
    /// label the settle. `None` means the leader exited on its own.
    termination: Option<Termination>,
    ring: Ring,
    /// The model's read cursor: an absolute byte offset into everything the job produced.
    read_offset: u64,
    /// The process group id (the leader's pid).
    pgid: u32,
    /// How many `job_output wait=true` calls are blocked on this job right now. A settle with
    /// a waiter is one the model already gets through the wait, so it is not notified again.
    waiters: usize,
    /// Shared with the supervisor: `true` once the leader has been reaped. Every signal is
    /// sent while holding this, so a reused group id can never be signalled.
    reaped: Arc<Mutex<bool>>,
    /// Flipped to `true` when the job settles; `job_output`'s `wait` awaits it.
    settled: tokio::sync::watch::Sender<bool>,
}

/// A job as `job_list` shows it.
#[derive(Debug, Clone)]
pub struct JobSummary {
    pub id: String,
    pub command: String,
    pub state: JobState,
    pub elapsed: Duration,
    /// [`OUTPUT_HINT`]; the output file is not readable by the model (ADR 0039 decision 1).
    pub output: String,
}

/// One job lifecycle notice for the host (ADR 0037 slices 3-4): the host writes the durable
/// start/settle records and, on a settle, decides whether to notify the model.
#[derive(Debug, Clone)]
pub enum JobEvent {
    Started {
        id: String,
        /// The command's first line.
        command: String,
        /// [`OUTPUT_HINT`] (ADR 0039 decision 1): the output file is outside the workspace and
        /// unreadable by the sandbox, so this is not a path.
        output: String,
    },
    Settled {
        id: String,
        state: JobState,
        duration_ms: u64,
        /// Whether the model should be told: `false` for `job_kill`/a session-exit settle and
        /// for a settle a `job_output wait` already returned (ADR 0037 decisions 2 and 4).
        notify: bool,
    },
}

/// The registry, per session.
pub struct Jobs {
    inner: Arc<JobsInner>,
}

struct JobsInner {
    /// The session-owned jobs directory (ADR 0039 decision 1). Every registry is given one;
    /// it is never derived from a workspace.
    dir: PathBuf,
    state: Mutex<JobsState>,
    events: tokio::sync::broadcast::Sender<JobEvent>,
}

#[derive(Default)]
struct JobsState {
    /// Start order, for `job_list`.
    order: Vec<String>,
    jobs: HashMap<String, Arc<Mutex<JobCell>>>,
}

impl Jobs {
    /// A registry whose output files live in `dir` (ADR 0039 decision 1). The caller owns the
    /// directory choice: the host passes the session's own jobs directory, so the registry
    /// never computes a workspace path. The directory is created 0700 on the first
    /// [`Jobs::start`], so opening a session never touches the disk.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let (events, _) = tokio::sync::broadcast::channel(SETTLED_CAPACITY);
        Self {
            inner: Arc::new(JobsInner {
                dir: dir.into(),
                state: Mutex::new(JobsState::default()),
                events,
            }),
        }
    }

    /// The directory this registry writes its output files to.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The output file for `id`. The model never sees this path (it cannot read the file); it
    /// is here for the host and for tests.
    pub fn output_path(&self, id: &str) -> PathBuf {
        self.inner.dir.join(format!("{id}.out"))
    }

    /// Subscribe to job lifecycle notices (ADR 0037 slices 3-4). Each receiver sees every
    /// notice that happens after it subscribes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<JobEvent> {
        self.inner.events.subscribe()
    }

    /// Starts `command` in the background under `sandbox`. One approval covers its whole life
    /// (the caller has already been approved for this tool call). `timeout` is the optional
    /// `timeout_secs`; without it a background job has **no** timeout.
    pub fn start(
        &self,
        sandbox: &Sandbox,
        cwd: &Path,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<StartInfo, ToolFailure> {
        let mut state = self.lock_state();
        let running = state
            .jobs
            .values()
            .filter(|cell| lock(cell).state.is_running())
            .count();
        if running >= MAX_RUNNING_JOBS {
            return Err(fail(
                "too_many_jobs",
                format!(
                    "this session already has {running} background jobs running (the limit is \
                     {MAX_RUNNING_JOBS}). Wait for one to finish with job_output wait=true, or \
                     stop one with job_kill, or list them with job_list."
                ),
            ));
        }

        ensure_jobs_dir(&self.inner.dir).map_err(|e| {
            fail(
                "jobs_dir",
                format!(
                    "could not create the jobs directory {}: {e}",
                    self.inner.dir.display()
                ),
            )
        })?;
        let id = next_job_id();
        let output = self.output_path(&id);
        let file = open_job_output(&output).map_err(|e| {
            fail(
                "jobs_output",
                format!("could not create {}: {e}", output.display()),
            )
        })?;

        let mut process = sandboxed_command(sandbox, cwd, command)?;
        configure_group(&mut process);
        let mut child = process.spawn().map_err(|e| {
            fail(
                "shell_spawn_failed",
                format!("could not start the command: {e}"),
            )
        })?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let (settled, _) = tokio::sync::watch::channel(false);
        let cell = Arc::new(Mutex::new(JobCell {
            command: first_line(command),
            started: Instant::now(),
            state: JobState::Running,
            termination: None,
            ring: Ring::default(),
            read_offset: 0,
            pgid: pid,
            reaped: Arc::new(Mutex::new(false)),
            waiters: 0,
            settled,
        }));

        let (done_tx, done_rx) = mpsc::channel();
        {
            let reader_cell = Arc::clone(&cell);
            let reader_file = file.try_clone().map_err(|e| {
                fail(
                    "jobs_output",
                    format!("could not duplicate the output file: {e}"),
                )
            })?;
            let done = done_tx.clone();
            thread::spawn(move || {
                read_tee(stdout, reader_file, reader_cell);
                let _ = done.send(());
            });
        }
        {
            let reader_cell = Arc::clone(&cell);
            let done = done_tx;
            thread::spawn(move || {
                read_tee(stderr, file, reader_cell);
                let _ = done.send(());
            });
        }

        // ADR 0039 decision 6: register the job and publish `Started` **before** the supervisor
        // can publish a `Settled`. An instant command's leader can be reaped the moment the
        // supervisor starts, so starting it any earlier lets `Settled` overtake `Started` in
        // the journal. The readers are already running, so nothing is lost by the wait.
        state.order.push(id.clone());
        state.jobs.insert(id.clone(), Arc::clone(&cell));
        drop(state);
        let _ = self.inner.events.send(JobEvent::Started {
            id: id.clone(),
            command: first_line(command),
            output: OUTPUT_HINT.to_owned(),
        });

        {
            let supervisor_cell = Arc::clone(&cell);
            let events = self.inner.events.clone();
            let job_id = id.clone();
            thread::spawn(move || supervise(pid, child, supervisor_cell, done_rx, events, job_id));
        }
        if let Some(timeout) = timeout {
            let timer_cell = Arc::clone(&cell);
            thread::spawn(move || {
                thread::sleep(timeout);
                terminate(&timer_cell, Termination::TimedOut);
            });
        }

        Ok(StartInfo {
            id,
            output: OUTPUT_HINT.to_owned(),
        })
    }

    /// `job_output`: the increment since the last call, plus the state. `wait` blocks until
    /// the job settles, `timeout` elapses or the turn is cancelled; a wait timeout never kills
    /// the job, and neither does a cancellation (ADR 0037 decision 6).
    pub(crate) async fn output(
        &self,
        id: &str,
        wait: bool,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<OutputResult, ToolFailure> {
        let cell = self.get(id)?;
        let mut waited_out = false;
        let mut wait_cancelled = false;
        if wait {
            let mut rx = {
                let mut cell = lock(&cell);
                cell.waiters += 1;
                cell.settled.subscribe()
            };
            let _waiter = WaiterGuard(Arc::clone(&cell));
            if !*rx.borrow_and_update() {
                tokio::select! {
                    _ = rx.changed() => {}
                    _ = tokio::time::sleep(timeout) => waited_out = true,
                    _ = cancel.cancelled() => wait_cancelled = true,
                }
            }
        }
        let mut cell = lock(&cell);
        let (dropped, increment) = cell.ring.since(cell.read_offset);
        cell.read_offset = cell.ring.total;
        let result = OutputResult {
            state: cell.state.clone(),
            dropped,
            increment,
            waited_out,
            wait_cancelled,
            output: OUTPUT_HINT.to_owned(),
        };
        drop(cell);
        Ok(result)
    }

    /// `job_list`: every job of this session, in start order.
    pub fn list(&self) -> Vec<JobSummary> {
        let state = self.lock_state();
        state
            .order
            .iter()
            .filter_map(|id| {
                let cell = state.jobs.get(id)?;
                let cell = lock(cell);
                Some(JobSummary {
                    id: id.clone(),
                    command: cell.command.clone(),
                    state: cell.state.clone(),
                    elapsed: cell.started.elapsed(),
                    output: OUTPUT_HINT.to_owned(),
                })
            })
            .collect()
    }

    /// Stop one job by id for the `/jobs` command (ADR 0037 decision 4). `Ok(None)` means it
    /// was running and the kill was requested; `Ok(Some(state))` means it had already settled.
    pub fn stop(&self, id: &str) -> Result<Option<String>, ToolFailure> {
        match self.kill(id)? {
            KillResult::Requested => Ok(None),
            KillResult::AlreadySettled(state) => Ok(Some(state.label())),
        }
    }

    /// `job_kill`: SIGTERM the group, SIGKILL after the grace. A settled job is a no-op.
    pub(crate) fn kill(&self, id: &str) -> Result<KillResult, ToolFailure> {
        let cell = self.get(id)?;
        let settled = {
            let cell = lock(&cell);
            if !cell.state.is_running() {
                Some(cell.state.clone())
            } else {
                None
            }
        };
        match settled {
            Some(state) => Ok(KillResult::AlreadySettled(state)),
            None => {
                terminate(&cell, Termination::Killed);
                Ok(KillResult::Requested)
            }
        }
    }

    /// The tail of a job's output plus its state and relative output path, for the completion
    /// message (ADR 0037 decision 5). `None` for an unknown id.
    pub fn tail(&self, id: &str, max: usize) -> Option<(JobState, String, String)> {
        let cell = self.get(id).ok()?;
        let cell = lock(&cell);
        Some((
            cell.state.clone(),
            OUTPUT_HINT.to_owned(),
            cell.ring.tail(max),
        ))
    }

    /// Session end: SIGKILL every running job's group. No file I/O (ADR 0037 decision 6), so
    /// it is safe on the panic path; the supervisor reaps each leader and settles the state.
    /// Session end: SIGKILL every running job's group. No file I/O (ADR 0037 decision 6), so
    /// it is safe on the panic path; the supervisor reaps each leader and settles the state.
    ///
    /// A session-exit cleanup is a **kill**: it marks `termination = Killed` so the settle is
    /// `killed` and `notify: false` (ADR 0037 decision 4: a session-exit settle is not
    /// notified). Without this the settle would read `signal 9` and notify.
    pub fn kill_all(&self) {
        let cells: Vec<Arc<Mutex<JobCell>>> = {
            let state = self.lock_state();
            state.jobs.values().cloned().collect()
        };
        for cell in cells {
            let (pgid, reaped) = {
                let mut cell = lock(&cell);
                if !cell.state.is_running() {
                    continue;
                }
                cell.termination = Some(Termination::Killed);
                (cell.pgid, Arc::clone(&cell.reaped))
            };
            signal_if_reserved(pgid, SIGKILL, &reaped);
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Mutex<JobCell>>, ToolFailure> {
        let state = self.lock_state();
        state.jobs.get(id).cloned().ok_or_else(|| {
            fail(
                "unknown_job",
                format!("no background job `{id}` in this session; use job_list to see them"),
            )
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, JobsState> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Jobs {
    /// The safety net for a session that is dropped without `close` (a panic unwind, an early
    /// return): kill every running group. `killpg` only, no file I/O.
    fn drop(&mut self) {
        self.kill_all();
    }
}

/// What `start` returns.
#[derive(Debug)]
pub struct StartInfo {
    pub id: String,
    /// [`OUTPUT_HINT`]: the model reads the output through `job_output` (ADR 0039 decision 1).
    pub output: String,
}

/// What `job_output` returns.
pub(crate) struct OutputResult {
    pub state: JobState,
    /// Bytes the ring had to drop before the caller's cursor.
    pub dropped: u64,
    pub increment: Vec<u8>,
    /// The wait hit its timeout: the job is still running.
    pub waited_out: bool,
    /// The turn was cancelled while waiting: the job is untouched.
    pub wait_cancelled: bool,
    /// [`OUTPUT_HINT`] (ADR 0039 decision 1).
    pub output: String,
}

/// What `job_kill` returns.
pub(crate) enum KillResult {
    Requested,
    AlreadySettled(JobState),
}

// ---------- output files ----------

/// Creates the jobs directory 0700 if it is missing. The host normally creates it with
/// `radian_session::ensure_private_dir` (ADR 0020) before the sandbox is built; this mirrors
/// that for a registry used without a host (tests, an embedded tool set) so the output is
/// owner-only either way.
fn ensure_jobs_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir_all(dir);
    match created {
        // Another thread or process won the race; the directory is there.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

/// Opens a job's output file for appending, creating it **exclusively** and refusing to follow
/// a symlink at the destination (ADR 0039 decision 1): `O_CREAT|O_EXCL|O_NOFOLLOW`, mode 0600.
///
/// `create_new` alone already refuses any existing path (including a symlink), so `O_NOFOLLOW`
/// is defence in depth for the platforms where the two are not equivalent; on macOS and Linux
/// the kernel reports `EEXIST` for both. The pair is what the ADR asks for.
fn open_job_output(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        options.mode(0o600);
    }
    options.open(path)
}

// ---------- output ----------

/// A bounded tail of a job's output. `start` is the absolute offset of `buf[0]`; `total` is
/// every byte the job produced.
#[derive(Default)]
struct Ring {
    buf: VecDeque<u8>,
    start: u64,
    total: u64,
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        self.total += bytes.len() as u64;
        while self.buf.len() > JOB_RING_BYTES {
            self.buf.pop_front();
            self.start += 1;
        }
    }

    /// The bytes from `cursor` on, plus how many bytes the ring dropped before the cursor.
    fn since(&self, cursor: u64) -> (u64, Vec<u8>) {
        if cursor >= self.total {
            return (0, Vec::new());
        }
        if cursor < self.start {
            (self.start - cursor, self.buf.iter().copied().collect())
        } else {
            let skip = (cursor - self.start) as usize;
            (0, self.buf.iter().skip(skip).copied().collect())
        }
    }

    /// The last `max` bytes of the ring, for the completion message's inline preview.
    fn tail(&self, max: usize) -> String {
        let skip = self.buf.len().saturating_sub(max);
        let bytes: Vec<u8> = self.buf.iter().skip(skip).copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Reads to EOF, appending to the job's output file and pushing into its ring. Runs on its own
/// thread per stream; the ring's lock is the only shared state.
fn read_tee(mut reader: impl Read, mut file: File, cell: Arc<Mutex<JobCell>>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let bytes = &buf[..n];
                let _ = file.write_all(bytes);
                let mut cell = lock(&cell);
                cell.ring.push(bytes);
            }
        }
    }
}

// ---------- supervision ----------

/// Puts the child in its own session and process group (`setsid`), with null stdin and piped
/// stdout/stderr. Same rule as the foreground executor (ADR 0016): no controlling terminal,
/// so the whole tree is reachable by `killpg` and `TIOCSTI` into the host cannot work.
fn configure_group(process: &mut Command) {
    process
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
}

/// Waits for the leader without reaping it, kills the whole group, reaps, drains the readers,
/// then records the settle. The `reaped` lock is held across the kill and the reap, so a
/// concurrent `job_kill` can never signal a reused group id.
fn supervise(
    pid: u32,
    mut child: Child,
    cell: Arc<Mutex<JobCell>>,
    done: mpsc::Receiver<()>,
    events: tokio::sync::broadcast::Sender<JobEvent>,
    id: String,
) {
    wait_for_exit_without_reaping(pid);
    let reaped = {
        let cell = lock(&cell);
        Arc::clone(&cell.reaped)
    };
    let status = {
        let mut guard = lock_bool(&reaped);
        // The leader is a zombie (or still running, if we were asked to kill): either way its
        // pid and group are still reserved.
        signal_group(pid, SIGKILL);
        let status = child.wait().ok();
        *guard = true;
        status
    };
    let _ = done.recv_timeout(READER_GRACE);
    let _ = done.recv_timeout(READER_GRACE);

    let (state, duration_ms, notify) = {
        let mut cell = lock(&cell);
        cell.state = match cell.termination {
            Some(Termination::Killed) => JobState::Killed,
            Some(Termination::TimedOut) => JobState::TimedOut,
            None => exit_state(status),
        };
        let state = cell.state.clone();
        let duration_ms = cell.started.elapsed().as_millis() as u64;
        // `job_kill` and a session-exit settle are not notified; a settle a waiter is already
        // blocked on is not notified again (ADR 0037 decisions 2 and 4). A timeout **is**
        // notified: the model set the timeout but did not ask to kill the job, so it may be
        // waiting on it.
        let notify = !matches!(cell.termination, Some(Termination::Killed)) && cell.waiters == 0;
        let _ = cell.settled.send(true);
        (state, duration_ms, notify)
    };
    let _ = events.send(JobEvent::Settled {
        id,
        state,
        duration_ms,
        notify,
    });
}

/// Decrements a job's waiter count on drop, so a cancelled or panicked `job_output wait`
/// cannot leave a job marked as "already waited" forever.
struct WaiterGuard(Arc<Mutex<JobCell>>);

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut cell = lock(&self.0);
        cell.waiters = cell.waiters.saturating_sub(1);
    }
}

fn exit_state(status: Option<ExitStatus>) -> JobState {
    match status {
        Some(status) => match status.code() {
            Some(code) => JobState::Exited(code),
            None => JobState::Signaled(status_signal(status)),
        },
        None => JobState::Signaled(-1),
    }
}

#[cfg(unix)]
fn status_signal(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().unwrap_or(-1)
}

#[cfg(not(unix))]
fn status_signal(_status: ExitStatus) -> i32 {
    -1
}

/// Marks a running job as killed/timed out and stops its group: SIGTERM, then SIGKILL after
/// [`KILL_GRACE`]. A no-op on an already-settled job.
fn terminate(cell: &Arc<Mutex<JobCell>>, kind: Termination) {
    let (pgid, reaped) = {
        let mut cell = lock(cell);
        if !cell.state.is_running() {
            return;
        }
        cell.termination = Some(kind);
        (cell.pgid, Arc::clone(&cell.reaped))
    };
    signal_if_reserved(pgid, SIGTERM, &reaped);
    let reaped = Arc::clone(&reaped);
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        signal_if_reserved(pgid, SIGKILL, &reaped);
    });
}

/// Sends `signal` only while the leader is still unreaped, holding the same lock the
/// supervisor holds across its kill-and-reap, so a reused group id is never signalled.
fn signal_if_reserved(pgid: u32, signal: i32, reaped: &Arc<Mutex<bool>>) {
    let guard = lock_bool(reaped);
    if !*guard {
        signal_group(pgid, signal);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

/// A process-unique, readable job id (`job-<pid>-<n>`).
fn next_job_id() -> String {
    format!(
        "job-{}-{}",
        std::process::id(),
        NEXT_JOB.fetch_add(1, Ordering::SeqCst)
    )
}

fn first_line(command: &str) -> String {
    let line = command.lines().next().unwrap_or("").trim();
    if line.chars().count() > 120 {
        let mut out: String = line.chars().take(119).collect();
        out.push('…');
        out
    } else {
        line.to_owned()
    }
}

// ---------- tools ----------

/// The `job_output` spec.
pub(crate) fn output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "job_id": {
                "type": "string",
                "description": "The id `shell` returned when the job started."
            },
            "wait": {
                "type": "boolean",
                "description": "Block until the job ends (or timeout_ms elapses). Default \
                                false: return the output so far at once."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "description": "How long `wait` may block, in milliseconds (default 30000, \
                                maximum 600000). On timeout the job keeps running."
            }
        },
        "required": ["job_id"],
        "additionalProperties": false
    })
}

pub(crate) fn output_spec() -> ToolSpec {
    ToolSpec {
        name: "job_output".into(),
        description: "Read what a background `shell` job has produced **since your last \
                      job_output call**, then a `[status: ...]` line (running / exited n / \
                      signal n / killed / timed out). Do not sleep or poll: pass `wait: true` \
                      to block until the job ends (or `timeout_ms` elapses; the default is \
                      30000, the maximum 600000). A wait timeout returns `[status: running]` \
                      and does **not** kill the job. This is the only way to read a background \
                      job's output: its file is outside the workspace and not readable from \
                      inside the sandbox, so `job_output` is what delivers it."
            .into(),
        input_schema: output_schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

/// The `job_list` spec.
pub(crate) fn list_spec() -> ToolSpec {
    ToolSpec {
        name: "job_list".into(),
        description: "List this session's background `shell` jobs: id, the command's first \
                      line, its state (running / exited n / signal n / killed / timed out), and \
                      how long it has run. Use `job_output` to read one and `job_kill` to stop \
                      one."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }
}

pub(crate) fn kill_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "job_id": {
                "type": "string",
                "description": "The id `shell` returned when the job started."
            }
        },
        "required": ["job_id"],
        "additionalProperties": false
    })
}

/// The `job_kill` spec.
pub(crate) fn kill_spec() -> ToolSpec {
    ToolSpec {
        name: "job_kill".into(),
        description: "Stop a background `shell` job: SIGTERM to its whole process group, then \
                      SIGKILL after 2 seconds. The job settles as killed. Killing a job that \
                      has already finished is a no-op that says so."
            .into(),
        input_schema: kill_schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

#[derive(Deserialize)]
struct JobIdArgs {
    job_id: String,
}

#[derive(Deserialize)]
struct OutputArgs {
    job_id: String,
    wait: Option<bool>,
    timeout_ms: Option<u64>,
}

pub(crate) async fn run_output(
    jobs: &Jobs,
    arguments: &serde_json::Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let args: OutputArgs = args::parse("job_output", &output_schema(), arguments)?;
    let timeout = Duration::from_millis(
        args.timeout_ms
            .unwrap_or(DEFAULT_WAIT_MS)
            .clamp(1, MAX_WAIT_MS),
    );
    let result = jobs
        .output(
            &args.job_id,
            args.wait.unwrap_or(false),
            timeout,
            &cx.cancellation,
        )
        .await?;

    let mut text = String::new();
    if result.increment.is_empty() {
        text.push_str("[no new output]\n");
    } else {
        text.push_str(&String::from_utf8_lossy(&result.increment));
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
    if result.dropped > 0 {
        text.push_str(&format!(
            "[{} bytes dropped before this read; only the most recent output is available]\n",
            result.dropped
        ));
    }
    if result.waited_out {
        text.push_str(&format!(
            "[still running after {} ms; the job was not killed]\n",
            timeout.as_millis()
        ));
    }
    if result.wait_cancelled {
        text.push_str("[the turn was cancelled; the job keeps running]\n");
    }
    text.push_str(&format!("[status: {}]\n", result.state.label()));

    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "job_id": args.job_id,
            "state": result.state.label(),
            "running": result.state.is_running(),
            "output": result.output,
            "new_bytes": result.increment.len(),
            "dropped_bytes": result.dropped,
        })),
    })
}

pub(crate) fn run_list(jobs: &Jobs) -> Result<ToolOutput, ToolFailure> {
    let jobs = jobs.list();
    let mut text = String::new();
    if jobs.is_empty() {
        text.push_str("no background jobs in this session\n");
    }
    for job in &jobs {
        text.push_str(&format!(
            "{}  {}  {}  {}\n  output: {}\n",
            job.id,
            job.state.label(),
            human_elapsed(job.elapsed),
            job.command,
            job.output
        ));
    }
    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "count": jobs.len(),
            "jobs": jobs
                .iter()
                .map(|job| json!({
                    "job_id": job.id,
                    "state": job.state.label(),
                    "running": job.state.is_running(),
                    "elapsed_ms": job.elapsed.as_millis() as u64,
                    "command": job.command,
                    "output": job.output,
                }))
                .collect::<Vec<_>>(),
        })),
    })
}

pub(crate) fn run_kill(
    jobs: &Jobs,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: JobIdArgs = args::parse("job_kill", &kill_schema(), arguments)?;
    let (text, state) = match jobs.kill(&args.job_id)? {
        KillResult::Requested => (
            format!(
                "stopping {} (SIGTERM, then SIGKILL after 2s); it settles as killed\n",
                args.job_id
            ),
            None,
        ),
        KillResult::AlreadySettled(state) => (
            format!("{} already finished ({})\n", args.job_id, state.label()),
            Some(state.label()),
        ),
    };
    Ok(ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "job_id": args.job_id,
            "requested": state.is_none(),
            "state": state,
        })),
    })
}

fn human_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_last_bytes_and_reports_the_increment() {
        let mut ring = Ring::default();
        ring.push(b"hello");
        assert_eq!(ring.since(0), (0, b"hello".to_vec()));
        assert_eq!(ring.since(5), (0, Vec::new()));
        // The next read only sees what arrived after the cursor.
        ring.push(b" world");
        assert_eq!(ring.since(5), (0, b" world".to_vec()));
        assert_eq!(ring.since(0), (0, b"hello world".to_vec()));
    }

    #[test]
    fn the_ring_bounds_its_memory_and_reports_dropped_bytes() {
        let mut ring = Ring::default();
        ring.push(&vec![b'x'; JOB_RING_BYTES + 100]);
        assert_eq!(ring.buf.len(), JOB_RING_BYTES);
        assert_eq!(ring.start, 100);
        assert_eq!(ring.total, (JOB_RING_BYTES + 100) as u64);
        // A cursor before the ring's start sees the dropped count and the whole ring.
        let (dropped, bytes) = ring.since(0);
        assert_eq!(dropped, 100);
        assert_eq!(bytes.len(), JOB_RING_BYTES);
        // A cursor inside the ring sees only the tail.
        let (dropped, bytes) = ring.since(150);
        assert_eq!(dropped, 0);
        assert_eq!(bytes.len(), JOB_RING_BYTES - 50);
    }

    #[test]
    fn a_long_command_is_summarised_by_its_first_line() {
        assert_eq!(first_line("make test\necho done"), "make test");
        assert_eq!(first_line("  spaced  "), "spaced");
        assert_eq!(first_line(&"x".repeat(200)).chars().count(), 120);
    }
}
