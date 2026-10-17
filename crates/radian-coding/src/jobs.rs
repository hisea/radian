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
//! The session invariant is the same one [`crate::shell`] uses: the leader is kept
//! **unreaped** while its session is reserved, and every signal is sent under the `reaped`
//! mutex, so a session id can never be reused under a kill. A stop signals the leader's whole
//! **session**, not only its process group (a child that calls `setpgid`, like GNU `timeout`,
//! leaves the group but keeps the session): `job_kill` sends SIGTERM, then SIGKILL after
//! [`KILL_GRACE`]; a natural exit, a timeout or a session-end cleanup stops the whole session
//! the same way. Killing never touches the `.out` file.
//!
//! Slice 3 will subscribe to settled jobs through [`Jobs::subscribe`]; this slice does not
//! wire the inbox.

use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
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
use crate::provider::Limits;
use crate::shell::{
    Supervised, lock_bool, own_session, sandboxed_command, signal_session,
    wait_for_exit_without_reaping,
};

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
pub(crate) struct JobCell {
    /// The command's first line (the summary the list shows).
    command: String,
    started: Instant,
    state: JobState,
    /// Set by `job_kill`/the timeout before the session is signalled, so the supervisor can
    /// label the settle. `None` means the leader exited on its own.
    termination: Option<Termination>,
    ring: Ring,
    /// The model's read cursor: an absolute byte offset into everything the job produced.
    read_offset: u64,
    /// The registry-computed output file (ADR 0039 decision 1). `job_output`'s file arguments
    /// read this path; it is never built from a model-supplied string.
    output: PathBuf,
    /// The session id, which is also the process group id: the leader's pid, since it called
    /// `setsid` (this is what a stop signals, see [`crate::shell::signal_session`]).
    sid: u32,
    /// How many `job_output wait=true` calls are blocked on this job right now. A settle with
    /// a waiter is one the model already gets through the wait, so it is not notified again.
    waiters: usize,
    /// Shared with the supervisor: `true` once the leader has been reaped. Every signal is
    /// sent while holding this, so a reused session id can never be signalled.
    reaped: Arc<Mutex<bool>>,
    /// Flipped to `true` when the job settles; `job_output`'s `wait` awaits it.
    settled: tokio::sync::watch::Sender<bool>,
}

impl JobCell {
    /// Appends post-adoption bytes to the job's `job_output` ring. Used by a promoted
    /// foreground stream (ADR 0037); the ring itself stays private to this module.
    pub(crate) fn push_ring(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
    }
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
    /// directory choice: the host passes the session's own jobs directory
    /// (`<sessions_dir>/<session-id>.jobs/`, created 0700 with `radian_session::ensure_private_dir`),
    /// so the registry never computes a workspace path. If the directory is missing (a registry
    /// built without a host), the first [`Jobs::start`] creates it 0700.
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
            output: output.clone(),
            sid: pid,
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

    /// Adopts a **foreground** command that is still running (ADR 0037): the same
    /// child and the same supervisor become a job, in place. `supervised`'s two stream sinks
    /// are promoted to the job's `.out` file and ring; the supervisor (owned by
    /// `crate::shell`) settles the job once it reaps. Nothing is restarted.
    ///
    /// `remaining_timeout` is the explicit `timeout_secs` still to run, measured from the
    /// command's start; the job is killed like `job_kill` and settles as timed out then.
    /// `None` means no deadline after adoption.
    ///
    /// Returns [`AdoptInfo`] with the job id and the pre-adoption output. When all
    /// [`MAX_RUNNING_JOBS`] slots are busy this is a `too_many_jobs` error and **nothing** is
    /// changed (decision 4): the caller runs the command to its foreground timeout.
    pub(crate) fn adopt(
        &self,
        supervised: &Supervised,
        command: &str,
        started: Instant,
        remaining_timeout: Option<Duration>,
    ) -> Result<AdoptInfo, ToolFailure> {
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
                     {MAX_RUNNING_JOBS}), so this foreground command was not moved to the \
                     background; it runs to its foreground timeout. Wait for one with \
                     job_output wait=true, or stop one with job_kill, or list them with \
                     job_list."
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
        let file_err = file.try_clone().map_err(|e| {
            fail(
                "jobs_output",
                format!("could not duplicate the output file: {e}"),
            )
        })?;

        let (settled, _) = tokio::sync::watch::channel(false);
        let cell = Arc::new(Mutex::new(JobCell {
            command: first_line(command),
            started,
            state: JobState::Running,
            termination: None,
            ring: Ring::default(),
            read_offset: 0,
            output: output.clone(),
            sid: supervised.pid,
            // The **same** reaped flag as the foreground supervisor, so `job_kill`/
            // `kill_all` signal under the exact lock the reaping holds (the one kill path).
            reaped: Arc::clone(&supervised.reaped),
            waiters: 0,
            settled,
        }));

        // Promote both sinks while holding both locks: no reader can push, so the retained
        // pre-adoption bytes reach the file exactly once and the ring origin is exact.
        // `read_offset` starts at the adoption point, so the tail already in the tool result
        // is not repeated.
        let (adopted_bytes, stdout, stderr) = {
            let mut out = lock(&supervised.stdout);
            let mut err = lock(&supervised.stderr);
            let (out_bytes, stdout) = out.promote(file, Arc::clone(&cell));
            let (err_bytes, stderr) = err.promote(file_err, Arc::clone(&cell));
            let at = out_bytes + err_bytes;
            {
                let mut cell = lock(&cell);
                cell.ring.set_origin(at);
                cell.read_offset = at;
            }
            (at, stdout, stderr)
        };

        // Register and publish `Started` before the supervisor can settle (ADR 0039 decision
        // 6's ordering rule), then install the settle handle.
        state.order.push(id.clone());
        state.jobs.insert(id.clone(), Arc::clone(&cell));
        drop(state);
        let _ = self.inner.events.send(JobEvent::Started {
            id: id.clone(),
            command: first_line(command),
            output: OUTPUT_HINT.to_owned(),
        });
        *supervised.adopted.lock().unwrap_or_else(|e| e.into_inner()) = Some(AdoptedHandle {
            cell: Arc::clone(&cell),
            events: self.inner.events.clone(),
            id: id.clone(),
        });
        // The process may have exited exactly at the threshold: the supervisor then sent
        // `done` and found no handle, so settle here (exactly one take wins).
        if *supervised.done.borrow() {
            let handle = supervised
                .adopted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(handle) = handle {
                let status = supervised
                    .outcome
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|outcome| outcome.status);
                handle.settle(status);
            }
        }

        // The explicit `timeout_secs` keeps counting from the command's start (decision 3).
        if let Some(remaining) = remaining_timeout {
            let timer_cell = Arc::clone(&cell);
            thread::spawn(move || {
                thread::sleep(remaining);
                terminate(&timer_cell, Termination::TimedOut);
            });
        }

        Ok(AdoptInfo {
            id,
            output: OUTPUT_HINT.to_owned(),
            stdout,
            stderr,
            adopted_bytes,
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

    /// `job_kill`: SIGTERM the session, SIGKILL after the grace. A settled job is a no-op.
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

    /// Session end: SIGKILL every running job's whole session. No file I/O (ADR 0037
    /// decision 6), so it is safe on the panic path; the supervisor reaps each leader and
    /// settles the state.
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
            let (sid, reaped) = {
                let mut cell = lock(&cell);
                if !cell.state.is_running() {
                    continue;
                }
                cell.termination = Some(Termination::Killed);
                (cell.sid, Arc::clone(&cell.reaped))
            };
            signal_if_reserved(sid, SIGKILL, &reaped);
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
    /// return): kill every running session. Session-wide signals only, no file I/O.
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

/// What adopting a running foreground command returns (ADR 0037). The `stdout`
/// and `stderr` are the bounded rendered tail of the output produced **before** adoption; the
/// same bytes are already at the start of the job's output file.
#[derive(Debug)]
pub(crate) struct AdoptInfo {
    pub id: String,
    /// [`OUTPUT_HINT`].
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    /// Bytes of pre-adoption output written to the job's file (the ring origin).
    pub adopted_bytes: u64,
}

/// The settle handle a foreground supervisor takes once it reaps (ADR 0037). It
/// owns the job cell and the event sender so the supervisor can settle without knowing the
/// registry.
pub(crate) struct AdoptedHandle {
    cell: Arc<Mutex<JobCell>>,
    events: tokio::sync::broadcast::Sender<JobEvent>,
    id: String,
}

impl AdoptedHandle {
    /// Settles the adopted job with the leader's exit status. Called exactly once, by whichever
    /// of the supervisor and [`Jobs::adopt`] wins the take.
    pub(crate) fn settle(self, status: Option<ExitStatus>) {
        settle(&self.cell, &self.events, self.id, status);
    }
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
/// `radian_session::ensure_private_dir` when the session opens; this mirrors that for a registry
/// used without a host (tests, an embedded tool set) so the output is owner-only either way.
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
/// `create_new` already refuses an existing path, and a symlink is an existing path, so
/// `O_NOFOLLOW` is defence in depth here: when the destination is a symlink the two are
/// equivalent, which `job_output_security` pins. The pair is what the ADR asks for.
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
    /// A ring whose absolute origin is `at` (ADR 0037): bytes before `at` live in
    /// the job's output file, not the ring, and a cursor at `at` begins the post-adoption
    /// increments.
    fn set_origin(&mut self, at: u64) {
        self.buf.clear();
        self.start = at;
        self.total = at;
    }

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
    own_session(process);
}

/// Waits for the leader without reaping it, kills the whole session, reaps, drains the readers,
/// then records the settle. The `reaped` lock is held across the kill and the reap, so a
/// concurrent `job_kill` can never signal a reused session id.
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
        // pid, its group and its session are still reserved.
        signal_session(pid, SIGKILL);
        let status = child.wait().ok();
        *guard = true;
        status
    };
    let _ = done.recv_timeout(READER_GRACE);
    let _ = done.recv_timeout(READER_GRACE);

    settle(&cell, &events, id, status);
}

/// Records a job's settle from the leader's exit status and publishes one [`JobEvent::Settled`]
/// (ADR 0037 decisions 2 and 4). Shared by a background job's own supervisor and the
/// foreground supervisor of an adopted job (ADR 0037).
fn settle(
    cell: &Arc<Mutex<JobCell>>,
    events: &tokio::sync::broadcast::Sender<JobEvent>,
    id: String,
    status: Option<ExitStatus>,
) {
    let (state, duration_ms, notify) = {
        let mut cell = lock(cell);
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

/// Marks a running job as killed/timed out and stops its session: SIGTERM, then SIGKILL after
/// [`KILL_GRACE`]. A no-op on an already-settled job.
fn terminate(cell: &Arc<Mutex<JobCell>>, kind: Termination) {
    let (sid, reaped) = {
        let mut cell = lock(cell);
        if !cell.state.is_running() {
            return;
        }
        cell.termination = Some(kind);
        (cell.sid, Arc::clone(&cell.reaped))
    };
    signal_if_reserved(sid, SIGTERM, &reaped);
    let reaped = Arc::clone(&reaped);
    thread::spawn(move || {
        thread::sleep(KILL_GRACE);
        signal_if_reserved(sid, SIGKILL, &reaped);
    });
}

/// Sends `signal` to the job's whole session only while the leader is still unreaped, holding
/// the same lock the supervisor holds across its kill-and-reap, so a reused session id is never
/// signalled.
fn signal_if_reserved(sid: u32, signal: i32, reaped: &Arc<Mutex<bool>>) {
    let guard = lock_bool(reaped);
    if !*guard {
        signal_session(sid, signal);
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
                                false: return the output so far at once. Ignored by the file \
                                arguments below."
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "description": "How long `wait` may block, in milliseconds (default 30000, \
                                maximum 600000). On timeout the job keeps running."
            },
            "from_line": {
                "type": "integer",
                "minimum": 1,
                "description": "1-based line of the job's **full output file** to start at. \
                                Use with max_lines to page past what the ring kept."
            },
            "max_lines": {
                "type": "integer",
                "minimum": 1,
                "description": "Maximum full-file lines to return from from_line."
            },
            "tail_lines": {
                "type": "integer",
                "minimum": 1,
                "description": "Return the last N lines of the full output file."
            },
            "pattern": {
                "type": "string",
                "description": "Return only full-file lines matching this Rust regex (same \
                                convention as `grep`), numbered and capped."
            },
            "mode": {
                "type": "string",
                "enum": ["new", "tail", "range", "pattern"],
                "description": "Which read this is: `new` (the output since your last call; \
                                `wait`/`timeout_ms` apply), `tail` (`tail_lines`), `range` \
                                (`from_line`/`max_lines`) or `pattern` (`pattern`). With \
                                `mode`, the other reads' fields are ignored; without it, give \
                                at most one of them."
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
                      and does **not** kill the job. The job's output file is outside the \
                      workspace and not readable from inside the sandbox, so this tool is the \
                      only way to read it. The incremental read above is a bounded ring; when \
                      it has dropped bytes, read the full file by line with `tail_lines` (the \
                      last N), `from_line`/`max_lines` (a range, numbered like `read`), or \
                      `pattern` (only matching lines). The file result names the file's total \
                      line count and whether more remains, so you can page."
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
        description: "Stop a background `shell` job: SIGTERM to its whole session (every \
                      process the job started, including a child that left its process group), \
                      then SIGKILL after 2 seconds. The job settles as killed. Killing a job \
                      that has already finished is a no-op that says so."
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
    /// 1-based line of the full output file to start at.
    from_line: Option<u64>,
    /// Maximum full-file lines to return from `from_line`.
    max_lines: Option<u64>,
    /// The last N lines of the full output file.
    tail_lines: Option<u64>,
    /// Only full-file lines matching this Rust regex.
    pattern: Option<String>,
    /// The read the model names: with it, the other reads' fields
    /// are ignored. GPT fills every optional field (`pattern: ""`, `tail_lines: 1`,
    /// `from_line: 1`, ...), which the "at most one" rule below refused six times in one lead
    /// session; naming the read removes the guess.
    mode: Option<String>,
}

/// Which part of the **full** output file a `job_output` call asks for (ADR 0039 decision 1: the
/// model reads the whole output through this tool). The three modes are mutually exclusive;
/// `None` is the unchanged incremental ring read.
enum FileMode {
    Range {
        from_line: u64,
        max_lines: Option<u64>,
    },
    Tail {
        lines: u64,
    },
    Pattern {
        source: String,
        regex: regex::Regex,
    },
}

impl FileMode {
    /// The file mode the arguments request, or `None` for the ring behaviour. `wait`/`timeout_ms`
    /// do not apply to a file mode: it reads the file as it stands right now.
    fn from_args(args: &OutputArgs, raw: &serde_json::Value) -> Result<Option<Self>, ToolFailure> {
        // `args::parse` already dropped an optional `""` (so `pattern: ""` is no pattern).
        let pattern = args.pattern.as_deref();
        match args.mode.as_deref() {
            Some("new") => return Ok(None),
            Some("tail") => {
                let Some(lines) = args.tail_lines else {
                    return Err(args::invalid(
                        "job_output",
                        raw,
                        ["`mode: tail` needs `tail_lines`"],
                    ));
                };
                return Self::tail(lines, raw).map(Some);
            }
            Some("range") => return Self::range(args.from_line, args.max_lines, raw).map(Some),
            Some("pattern") => {
                let Some(source) = pattern else {
                    return Err(args::invalid(
                        "job_output",
                        raw,
                        ["`mode: pattern` needs a non-empty `pattern`"],
                    ));
                };
                return Self::pattern(source).map(Some);
            }
            // The schema's enum refuses any other value before this point.
            _ => {}
        }
        let requested = [
            pattern.is_some(),
            args.tail_lines.is_some(),
            args.from_line.is_some() || args.max_lines.is_some(),
        ]
        .into_iter()
        .filter(|wanted| *wanted)
        .count();
        if requested == 0 {
            return Ok(None);
        }
        if requested > 1 {
            return Err(args::invalid(
                "job_output",
                raw,
                ["use only one of `pattern`, `tail_lines`, or `from_line`/`max_lines`"],
            ));
        }
        if let Some(lines) = args.tail_lines {
            return Self::tail(lines, raw).map(Some);
        }
        if let Some(source) = pattern {
            return Self::pattern(source).map(Some);
        }
        Self::range(args.from_line, args.max_lines, raw).map(Some)
    }

    fn tail(lines: u64, raw: &serde_json::Value) -> Result<Self, ToolFailure> {
        if lines < 1 {
            return Err(args::invalid(
                "job_output",
                raw,
                ["`tail_lines` must be at least 1"],
            ));
        }
        Ok(FileMode::Tail { lines })
    }

    fn pattern(source: &str) -> Result<Self, ToolFailure> {
        let regex = regex::Regex::new(source)
            .map_err(|error| fail("invalid_regex", format!("`{source}`: {error}")))?;
        Ok(FileMode::Pattern {
            source: source.to_owned(),
            regex,
        })
    }

    fn range(
        from_line: Option<u64>,
        max_lines: Option<u64>,
        raw: &serde_json::Value,
    ) -> Result<Self, ToolFailure> {
        let from_line = from_line.unwrap_or(1);
        if from_line < 1 {
            return Err(args::invalid(
                "job_output",
                raw,
                ["`from_line` must be at least 1"],
            ));
        }
        if let Some(lines) = max_lines
            && lines < 1
        {
            return Err(args::invalid(
                "job_output",
                raw,
                ["`max_lines` must be at least 1"],
            ));
        }
        Ok(FileMode::Range {
            from_line,
            max_lines,
        })
    }
}

pub(crate) async fn run_output(
    jobs: &Jobs,
    limits: &Limits,
    arguments: &serde_json::Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let args: OutputArgs = args::parse("job_output", &output_schema(), arguments)?;
    if let Some(mode) = FileMode::from_args(&args, arguments)? {
        return read_output_file(jobs, &args.job_id, mode, limits);
    }
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
            "[{} bytes dropped before this read; page the full file with tail_lines, \
             from_line/max_lines or pattern]\n",
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

// ---------- reading the full output file ----------

/// The most a single file line contributes before it is marked `[truncated]` (grep's cap).
const JOB_LINE_BYTES: usize = crate::grep::MAX_LINE_BYTES;

/// Opens a job's output file for reading. The path comes from the registry cell, never a
/// model-supplied string; `O_NOFOLLOW` refuses a symlink at the final component. That is
/// defence in depth — the host created the file itself with `O_EXCL` in a directory the model
/// cannot write — but it keeps the read path on the same rule as the write path.
fn open_job_output_read(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// The number of lines in `path` as the read tool numbers them: a trailing newline does not add
/// an empty final line, and a non-empty file with no final newline has one more line.
fn count_output_lines(path: &Path) -> std::io::Result<u64> {
    let mut reader = BufReader::new(open_job_output_read(path)?);
    let mut count = 0u64;
    let mut any = false;
    let mut last_was_newline = true;
    let mut buf = [0u8; 64 << 10];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        any = true;
        for &byte in &buf[..n] {
            if byte == b'\n' {
                count += 1;
                last_was_newline = true;
            } else {
                last_was_newline = false;
            }
        }
    }
    if any && !last_was_newline {
        count += 1;
    }
    Ok(count)
}

/// A line range of the full output file, already numbered `N: `.
struct RangeRead {
    content: String,
    start_line: u64,
    end_line: u64,
    returned: u64,
    next_line: Option<u64>,
}

/// Reads at most `max_lines` numbered lines from `from_line`, bounded by `budget` bytes. The
/// skip is constant memory; each line is capped at [`JOB_LINE_BYTES`].
fn read_output_range(
    path: &Path,
    from_line: u64,
    max_lines: Option<u64>,
    budget: usize,
) -> std::io::Result<RangeRead> {
    let mut reader = BufReader::new(open_job_output_read(path)?);
    crate::read::skip_lines(&mut reader, (from_line - 1) as i64)?;
    let mut content = String::new();
    let mut line: Vec<u8> = Vec::new();
    let mut lineno = from_line;
    let mut returned = 0u64;
    let mut next_line = None;
    loop {
        if let Some(max) = max_lines
            && returned >= max
        {
            next_line = Some(lineno);
            break;
        }
        let truncated = match crate::grep::read_line_capped(&mut reader, &mut line, JOB_LINE_BYTES)?
        {
            crate::grep::LineRead::Eof => break,
            crate::grep::LineRead::Complete => false,
            crate::grep::LineRead::TooLong => true,
        };
        let text = crate::grep::strip_line_end(&line);
        let entry = if truncated {
            format!("{lineno}: {text}[truncated]\n")
        } else {
            format!("{lineno}: {text}\n")
        };
        if content.len() + entry.len() > budget {
            next_line = Some(lineno);
            break;
        }
        content.push_str(&entry);
        returned += 1;
        lineno += 1;
    }
    Ok(RangeRead {
        content,
        start_line: from_line,
        end_line: if returned == 0 {
            from_line.saturating_sub(1)
        } else {
            lineno - 1
        },
        returned,
        next_line,
    })
}

/// Matching lines of the full output file, already numbered `N: `.
struct PatternRead {
    content: String,
    total_lines: u64,
    matches: u64,
    cut: bool,
}

/// Scans the whole file for `regex`, numbering the matching lines, capped at `cap` matches and
/// `budget` bytes. The scan continues to end of file so the result can name the total line
/// count and the model can page with `from_line`/`tail_lines`.
fn pattern_output(
    path: &Path,
    regex: &regex::Regex,
    budget: usize,
    cap: usize,
) -> std::io::Result<PatternRead> {
    let mut reader = BufReader::new(open_job_output_read(path)?);
    let mut content = String::new();
    let mut line: Vec<u8> = Vec::new();
    let mut lineno = 1u64;
    let mut matches = 0u64;
    let mut cut = false;
    loop {
        let truncated = match crate::grep::read_line_capped(&mut reader, &mut line, JOB_LINE_BYTES)?
        {
            crate::grep::LineRead::Eof => break,
            crate::grep::LineRead::Complete => false,
            crate::grep::LineRead::TooLong => true,
        };
        let text = crate::grep::strip_line_end(&line);
        if regex.is_match(&text) && !cut {
            if matches >= cap as u64 {
                cut = true;
            } else {
                let entry = if truncated {
                    format!("{lineno}: {text}[truncated]\n")
                } else {
                    format!("{lineno}: {text}\n")
                };
                if content.len() + entry.len() > budget {
                    cut = true;
                } else {
                    content.push_str(&entry);
                    matches += 1;
                }
            }
        }
        lineno += 1;
    }
    Ok(PatternRead {
        content,
        total_lines: lineno - 1,
        matches,
        cut,
    })
}

/// The read of a job's own output file failed. The path is not shown to the model.
fn read_failure(error: &std::io::Error) -> ToolFailure {
    fail(
        "jobs_output",
        format!("could not read the job's output file: {error}"),
    )
}

fn read_output_file(
    jobs: &Jobs,
    id: &str,
    mode: FileMode,
    limits: &Limits,
) -> Result<ToolOutput, ToolFailure> {
    // The id must be a real registry key; a path-like id is an unknown job, never a path.
    let cell = jobs.get(id)?;
    let path = lock(&cell).output.clone();
    let budget = limits.default_read_bytes;
    match mode {
        FileMode::Range {
            from_line,
            max_lines,
        } => {
            let total = count_output_lines(&path).map_err(|e| read_failure(&e))?;
            let range = read_output_range(&path, from_line, max_lines, budget)
                .map_err(|e| read_failure(&e))?;
            Ok(render_range(id, "range", range, total))
        }
        FileMode::Tail { lines } => {
            let total = count_output_lines(&path).map_err(|e| read_failure(&e))?;
            // 1-based: the last `lines` lines are `total - lines + 1 ..= total`.
            let start = total.saturating_sub(lines) + 1;
            let range = read_output_range(&path, start, Some(lines), budget)
                .map_err(|e| read_failure(&e))?;
            Ok(render_range(id, "tail", range, total))
        }
        FileMode::Pattern { source, regex } => {
            let scan = pattern_output(&path, &regex, budget, limits.default_grep_matches)
                .map_err(|e| read_failure(&e))?;
            Ok(render_pattern(id, &source, scan))
        }
    }
}

fn render_range(id: &str, mode: &str, range: RangeRead, total: u64) -> ToolOutput {
    let mut text = range.content.clone();
    if range.returned == 0 {
        text.push_str(&format!(
            "[no lines in that range of the output file ({total} lines total)]\n"
        ));
    } else {
        text.push_str(&format!(
            "[output file: {total} lines; showing lines {}-{}]\n",
            range.start_line, range.end_line
        ));
        match range.next_line {
            Some(next) => text.push_str(&format!(
                "[more remains: call job_output with from_line={next}]\n"
            )),
            None => text.push_str("[end of output file]\n"),
        }
    }
    let structured = json!({
        "job_id": id,
        "source": "file",
        "mode": mode,
        "total_lines": total,
        "start_line": range.start_line,
        "end_line": range.end_line,
        "returned": range.returned,
        "next_line": range.next_line,
        "truncated": range.next_line.is_some(),
    });
    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(structured),
    }
}

fn render_pattern(id: &str, source: &str, scan: PatternRead) -> ToolOutput {
    let mut text = scan.content.clone();
    if scan.matches == 0 {
        text.push_str(&format!("[no matches for `{source}`]\n"));
    }
    text.push_str(&format!(
        "[output file: {} lines; {} matching line(s) for `{source}`{}]\n",
        scan.total_lines,
        scan.matches,
        if scan.cut { "; more were cut" } else { "" }
    ));
    let structured = json!({
        "job_id": id,
        "source": "file",
        "mode": "pattern",
        "pattern": source,
        "total_lines": scan.total_lines,
        "matches": scan.matches,
        "truncated": scan.cut,
    });
    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(structured),
    }
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
