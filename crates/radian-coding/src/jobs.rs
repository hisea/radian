//! Background shell jobs (ADR 0037 slices 1-2).
//!
//! A background command runs under the **same sandbox and environment** as a foreground one
//! ([`crate::shell::sandboxed_command`]), in its own session and process group (`setsid`), but
//! the tool returns at once. Its output streams to `<id>.out` in the registry's jobs
//! directory (the full history) and into a bounded in-memory ring (the `job` read increments).
//! The registry is per session, owned by [`crate::provider::CodingSession`]; at most
//! [`MAX_RUNNING_JOBS`] run at once.
//!
//! The jobs directory is a **parameter**, never derived from the workspace (ADR 0039 decision
//! 1): the host passes the session's own directory (`<sessions_dir>/<session-id>.jobs/`,
//! 0700, on the sandbox's deny-read list), so a model that can write the workspace cannot
//! pre-plant a symlink the host would follow. The output file is opened
//! `O_CREAT|O_EXCL|O_NOFOLLOW` with mode 0600, and the model reads it only through
//! the `job` tool; the fields that used to carry a path carry [`OUTPUT_HINT`] instead.
//!
//! The session invariant is the same one [`crate::shell`] uses: the leader is kept
//! **unreaped** while its session is reserved, and every signal is sent under the `reaped`
//! mutex, so a session id can never be reused under a kill. A stop signals the leader's whole
//! **session**, not only its process group (a child that calls `setpgid`, like GNU `timeout`,
//! leaves the group but keeps the session): stopping a job sends SIGTERM, then SIGKILL after
//! [`KILL_GRACE`]; a natural exit, a timeout or a session-end cleanup stops the whole session
//! the same way. Killing never touches the `.out` file.
//!
//! Slice 3 will subscribe to settled jobs through [`Jobs::subscribe`]; this slice does not
//! wire the inbox.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
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
use crate::provider::Limits;
use crate::shell::{
    Supervised, lock_bool, own_session, sandboxed_command, signal_session,
    wait_for_exit_without_reaping,
};

/// How many background jobs may run at once in one session (ADR 0037 decision 7). A job costs
/// a process group, a file and a ring; four is enough for a build, a test run and a server,
/// and keeps the cost bounded.
pub const MAX_RUNNING_JOBS: usize = 4;

/// The in-memory ring per job, in bytes, for the `job` read increments. 64 KiB is enough for the
/// interesting tail of a build log between two tool calls; four jobs hold at most 256 KiB. The
/// `.out` file keeps the full history for the host, but the model reads only through
/// the `job` tool, so the ring is the read window and only has to cover one read gap.
pub const JOB_RING_BYTES: usize = 64 << 10;

/// SIGTERM, then SIGKILL after this grace (ADR 0037 decision 2).
const KILL_GRACE: Duration = Duration::from_secs(2);

/// The `job` wait's default, and its hard cap (dsh's numbers).
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
pub const OUTPUT_HINT: &str = "available through job";

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
    /// Stopping a job (or a session-end cleanup) stopped it.
    Killed,
    /// Its deadline elapsed (the adopted-job stop below). Only an adopted foreground call's
    /// `timeout_secs` sets one now (ADR 0037; revision 2 for explicit
    /// background jobs).
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
    /// The command's first line (the summary the TUI `/jobs` list shows).
    command: String,
    started: Instant,
    state: JobState,
    /// `true` for a retained foreground result (ADR 0037 decision 3a): a settled entry the
    /// model reads like any job, evicted with the foreground bounds, never counted as running.
    retained: bool,
    /// Set by a `job` kill/the timeout before the session is signalled, so the supervisor can
    /// label the settle. `None` means the leader exited on its own.
    termination: Option<Termination>,
    ring: Ring,
    /// The model's read cursor: an absolute byte offset into everything the job produced.
    read_offset: u64,
    /// The registry-computed output file (ADR 0039 decision 1). The `job` read's file arguments
    /// read this path; it is never built from a model-supplied string.
    output: PathBuf,
    /// The session id, which is also the process group id: the leader's pid, since it called
    /// `setsid` (this is what a stop signals, see [`crate::shell::signal_session`]).
    sid: u32,
    /// How many `job` waits are blocked on this job right now. A settle with
    /// a waiter is one the model already gets through the wait, so it is not notified again.
    waiters: usize,
    /// Shared with the supervisor: `true` once the leader has been reaped. Every signal is
    /// sent while holding this, so a reused session id can never be signalled.
    reaped: Arc<Mutex<bool>>,
    /// Flipped to `true` when the job settles; a `job` wait awaits it.
    settled: tokio::sync::watch::Sender<bool>,
}

impl JobCell {
    /// Appends post-adoption bytes to the job's output ring. Used by a promoted
    /// foreground stream (ADR 0037); the ring itself stays private to this module.
    pub(crate) fn push_ring(&mut self, bytes: &[u8]) {
        self.ring.push(bytes);
    }
}

/// A job as shown in a listing (the host's job list, the TUI footer).
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
        /// Whether the model should be told: `false` for a `job` kill/a session-exit settle and
        /// for a settle a `job` wait already returned (ADR 0037 decisions 2 and 4).
        notify: bool,
    },
}

/// The registry, per session.
pub struct Jobs {
    inner: Arc<JobsInner>,
    /// Harden git in every shell child this registry starts (ADR 0039 decision 5): the
    /// session's project is not trusted, so repository configuration must not run commands
    /// through a shell git. Set once at session setup; a trusted session leaves it off so
    /// the user's own hooks run.
    harden_git: bool,
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
    /// Start order, for listings.
    order: Vec<String>,
    jobs: HashMap<String, Arc<Mutex<JobCell>>>,
    /// How many background settles this registry has published. Bumped in the same
    /// critical section that flips the cell and sends the broadcast (see `settle`), so
    /// [`Jobs::settle_snapshot`] reads a consistent `(running, settled_total)` pair.
    settled_total: u64,
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
            harden_git: false,
        }
    }

    /// Harden git in every shell child this registry starts (ADR 0039 decision 5). The host
    /// sets it from the session's trust decision: an untrusted project hardens, a trusted
    /// one does not.
    pub fn with_git_hardening(mut self, on: bool) -> Self {
        self.harden_git = on;
        self
    }

    /// Whether shell children carry the host git hardening (ADR 0039 decision 5).
    pub(crate) fn git_hardening(&self) -> bool {
        self.harden_git
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
    /// (the caller has already been approved for this tool call). A background job has **no**
    /// timeout (ADR 0037: an explicit `background: true` ignores `timeout_secs`).
    pub fn start(
        &self,
        sandbox: &Sandbox,
        cwd: &Path,
        command: &str,
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
                     {MAX_RUNNING_JOBS}). Read one with `job` (`op: \"wait\"`) or stop one \
                     with `job` (`op: \"kill\"`)."
                ),
            ));
        }

        crate::output::ensure_private_dir(&self.inner.dir).map_err(|e| {
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
        let file = crate::output::open_output_file(&output).map_err(|e| {
            fail(
                "jobs_output",
                format!("could not create {}: {e}", output.display()),
            )
        })?;

        let mut process = sandboxed_command(sandbox, cwd, command, self.harden_git)?;
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
            retained: false,
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
            let inner = Arc::clone(&self.inner);
            let job_id = id.clone();
            thread::spawn(move || supervise(pid, child, supervisor_cell, done_rx, inner, job_id));
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
    /// command's start; the job is killed like a `job` kill and settles as timed out then.
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
                     background; it runs to its foreground timeout. Read one with `job` \
                     (`op: \"wait\"`) or stop one with `job` (`op: \"kill\"`)."
                ),
            ));
        }

        crate::output::ensure_private_dir(&self.inner.dir).map_err(|e| {
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
        let file = crate::output::open_output_file(&output).map_err(|e| {
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
            retained: false,
            termination: None,
            ring: Ring::default(),
            read_offset: 0,
            output: output.clone(),
            sid: supervised.pid,
            // The **same** reaped flag as the foreground supervisor, so a `job` kill or
            // `kill_all` signals under the exact lock the reaping holds (the one kill path).
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
            inner: Arc::clone(&self.inner),
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

    /// `job` with `op: read` and no range: the
    /// increment since the last call, plus the state. Never waits.
    pub(crate) fn read_increment(&self, id: &str) -> Result<OutputResult, ToolFailure> {
        let cell = self.get(id)?;
        let mut cell = lock(&cell);
        Ok(Self::take_increment(&mut cell, false, false))
    }

    /// `job` with `op: wait`: the increment since the last call, plus the state. `wait` blocks until
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
            // A settled job never waits: the state under the cell lock is the authority, and
            // `settle` writes it under the same lock as the watched value, so the two can never
            // disagree. The watched value alone would do now that a settle stores it
            // unconditionally, but this keeps the waiter count (and so the settle's `notify`)
            // off a job that has already finished.
            let rx = {
                let mut cell = lock(&cell);
                if cell.state.is_running() {
                    cell.waiters += 1;
                    Some(cell.settled.subscribe())
                } else {
                    None
                }
            };
            if let Some(mut rx) = rx {
                let _waiter = WaiterGuard(Arc::clone(&cell));
                if !*rx.borrow_and_update() {
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = tokio::time::sleep(timeout) => waited_out = true,
                        _ = cancel.cancelled() => wait_cancelled = true,
                    }
                }
            }
        }
        let mut cell = lock(&cell);
        Ok(Self::take_increment(&mut cell, waited_out, wait_cancelled))
    }

    /// Reads the ring increment under the cell lock and advances the cursor. Shared by the wait
    /// path and the plain incremental read.
    fn take_increment(cell: &mut JobCell, waited_out: bool, wait_cancelled: bool) -> OutputResult {
        let (dropped, increment) = cell.ring.since(cell.read_offset);
        cell.read_offset = cell.ring.total;
        OutputResult {
            state: cell.state.clone(),
            dropped,
            increment,
            waited_out,
            wait_cancelled,
            output: OUTPUT_HINT.to_owned(),
        }
    }

    /// How many background jobs are running right now, and how many settles this
    /// registry has published in total (both under one lock).
    ///
    /// The pair is consistent: `settle` flips the cell, bumps the total and sends the
    /// broadcast while holding this same lock, so a snapshot either sees a settle fully
    /// (the total includes it, and its broadcast was sent) or not at all (the job still
    /// counts as running). Retained foreground entries (ADR 0037 decision 3a) never run
    /// and never publish, so they affect neither number.
    pub fn settle_snapshot(&self) -> (usize, u64) {
        let registry = self.lock_state();
        let mut running = 0;
        for cell in registry.jobs.values() {
            if lock(cell).state.is_running() {
                running += 1;
            }
        }
        (running, registry.settled_total)
    }
    /// results (ADR 0037 decision 3a). The TUI `/jobs` command reads this, not a tool.
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

    /// Stops the job: SIGTERM the session, SIGKILL after the grace. A settled job is a no-op.
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

    /// Registers a finished foreground command's retained output as a settled entry (ADR
    /// 0037 decision 3a): a normal job id, the command's exit state, the same output file the
    /// foreground retention wrote. No events, no journal records, never counted as running.
    /// The caller owns the file's lifecycle (eviction, shutdown); this only owns the lookup.
    pub(crate) fn insert_settled(
        &self,
        id: String,
        command: String,
        state: JobState,
        output: PathBuf,
        ring: Ring,
    ) {
        debug_assert!(!state.is_running());
        let (settled, _) = tokio::sync::watch::channel(true);
        let cell = Arc::new(Mutex::new(JobCell {
            command: first_line(&command),
            started: Instant::now(),
            state,
            retained: true,
            termination: None,
            ring,
            read_offset: 0,
            output,
            sid: 0,
            reaped: Arc::new(Mutex::new(true)),
            waiters: 0,
            settled,
        }));
        let mut registry = self.lock_state();
        registry.order.push(id.clone());
        registry.jobs.insert(id, cell);
    }

    /// Drops a retained foreground entry, so its id reads as unknown (ADR 0037 decision 3a:
    /// eviction). Only retained entries can be removed; background jobs are never dropped.
    /// The caller removes the file.
    pub(crate) fn remove(&self, id: &str) {
        let mut registry = self.lock_state();
        let retained = registry
            .jobs
            .get(id)
            .is_some_and(|cell| lock(cell).retained);
        if retained {
            registry.jobs.remove(id);
            registry.order.retain(|kept| kept != id);
        }
    }

    fn get(&self, id: &str) -> Result<Arc<Mutex<JobCell>>, ToolFailure> {
        let state = self.lock_state();
        state.jobs.get(id).cloned().ok_or_else(|| {
            fail(
                "unknown_job",
                format!(
                    "no job `{id}` in this session; a truncated `shell` result carries its job id"
                ),
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
    /// [`OUTPUT_HINT`]: the model reads the output through the `job` tool (ADR 0039 decision 1).
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
/// owns the job cell and the registry so the supervisor can settle without knowing the
/// registry.
pub(crate) struct AdoptedHandle {
    cell: Arc<Mutex<JobCell>>,
    inner: Arc<JobsInner>,
    id: String,
}

impl AdoptedHandle {
    /// Settles the adopted job with the leader's exit status. Called exactly once, by whichever
    /// of the supervisor and [`Jobs::adopt`] wins the take.
    pub(crate) fn settle(self, status: Option<ExitStatus>) {
        settle(&self.inner, &self.cell, self.id, status);
    }
}

/// What a `job` read or wait returns.
pub(crate) struct OutputResult {
    pub state: JobState,
    /// Bytes the ring had to drop before the caller's cursor.
    pub dropped: u64,
    pub increment: Vec<u8>,
    /// The wait hit its timeout (it may have lost the race with a settle in that instant; the
    /// renderer prints "still running" only when [`OutputResult::state`] is running).
    pub waited_out: bool,
    /// The turn was cancelled while waiting: the job is untouched.
    pub wait_cancelled: bool,
    /// [`OUTPUT_HINT`] (ADR 0039 decision 1).
    pub output: String,
}

/// What a `job` kill returns.
pub(crate) enum KillResult {
    Requested,
    AlreadySettled(JobState),
}

// ---------- output ----------

/// A bounded tail of a job's output. `start` is the absolute offset of `buf[0]`; `total` is
/// every byte the job produced.
#[derive(Default)]
pub(crate) struct Ring {
    buf: VecDeque<u8>,
    start: u64,
    total: u64,
}

impl Ring {
    /// A ring holding the last bytes of `total` produced bytes (ADR 0037 decision 3a: a
    /// retained foreground entry's increment starts at byte 0, bounded by the ring). Longer
    /// input is cut from the front, and the cut is visible as `start > 0`.
    pub(crate) fn seeded(mut bytes: Vec<u8>, total: u64) -> Self {
        if bytes.len() > JOB_RING_BYTES {
            bytes.drain(..bytes.len() - JOB_RING_BYTES);
        }
        let start = total.saturating_sub(bytes.len() as u64);
        Ring {
            buf: bytes.into(),
            start,
            total,
        }
    }

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
/// concurrent kill can never signal a reused session id.
fn supervise(
    pid: u32,
    mut child: Child,
    cell: Arc<Mutex<JobCell>>,
    done: mpsc::Receiver<()>,
    inner: Arc<JobsInner>,
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

    settle(&inner, &cell, id, status);
}

/// Records a job's settle from the leader's exit status and publishes one [`JobEvent::Settled`]
/// (ADR 0037 decisions 2 and 4). Shared by a background job's own supervisor and the
/// foreground supervisor of an adopted job (ADR 0037).
fn settle(inner: &JobsInner, cell: &Arc<Mutex<JobCell>>, id: String, status: Option<ExitStatus>) {
    // Read-only under the cell lock: nothing flips yet, so this section commutes with the
    // snapshot below.
    let (state, duration_ms, notify) = {
        let cell = lock(cell);
        let state = match cell.termination {
            Some(Termination::Killed) => JobState::Killed,
            Some(Termination::TimedOut) => JobState::TimedOut,
            None => exit_state(status),
        };
        let duration_ms = cell.started.elapsed().as_millis() as u64;
        // A `job` kill and a session-exit settle are not notified; a settle a waiter is already
        // blocked on is not notified again (ADR 0037 decisions 2 and 4). A timeout **is**
        // notified: the model set the timeout but did not ask to kill the job, so it may be
        // waiting on it.
        let notify = !matches!(cell.termination, Some(Termination::Killed)) && cell.waiters == 0;
        (state, duration_ms, notify)
    };
    {
        // One critical section, the same lock `settle_snapshot` reads: flip the cell, count
        // the settle, then publish it. A snapshot either sees all three (the total includes
        // this settle and its broadcast is already sent) or none of them (the job still
        // counts as running). Lock order is registry-then-cell, the same as `list`.
        let mut registry = inner.state.lock().unwrap_or_else(|e| e.into_inner());
        lock(cell).state = state.clone();
        registry.settled_total += 1;
        let _ = inner.events.send(JobEvent::Settled {
            id,
            state,
            duration_ms,
            notify,
        });
    }
    // `send_replace`, not `send`: tokio's `send` returns `Err` **without storing** the value
    // when the channel has no receiver, and the receiver from `watch::channel(false)` is
    // dropped at once, so a job that settles while nobody waits would leave the watched
    // value `false` and a later `job` wait would sleep to its timeout.
    lock(cell).settled.send_replace(true);
}

/// Decrements a job's waiter count on drop, so a cancelled or panicked `job` wait
/// cannot leave a job marked as "already waited" forever.
struct WaiterGuard(Arc<Mutex<JobCell>>);

impl Drop for WaiterGuard {
    fn drop(&mut self) {
        let mut cell = lock(&self.0);
        cell.waiters = cell.waiters.saturating_sub(1);
    }
}

pub(crate) fn exit_state(status: Option<ExitStatus>) -> JobState {
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

/// A process-unique, readable job id (`job-<pid>-<n>`). Foreground retention mints from the
/// same counter (ADR 0037 decision 3a: one id space), so the two can never collide.
pub(crate) fn next_job_id() -> String {
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

/// The `job` spec: the model's only background tool (ADR 0037 decision 4). Its serialized
/// size is pinned by `crate::tests::size` (`shell` + `job` must stay small enough for the
/// cached prefix): keep every rule to one sentence and never repeat the description.
pub(crate) fn job_schema() -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "id".into(),
        json!({
            "type": "string",
            "description": "The job id `shell` returned."
        }),
    );
    properties.insert(
        "op".into(),
        json!({
            "type": "string",
            "enum": ["read", "wait", "kill"],
            "description": "`read` the output, `wait` for the end, or `kill` the job."
        }),
    );
    // The full-file read arguments (crate::output); only `read` uses them.
    if let Some(fields) = crate::output::file_properties().as_object() {
        for (key, value) in fields {
            properties.insert(key.clone(), value.clone());
        }
    }
    properties.insert(
        "timeout_ms".into(),
        json!({
            "type": "integer",
            "minimum": 1,
            "description": "How long `wait` may block, in milliseconds (default 30000, \
                            maximum 600000). On timeout the job keeps running."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": ["id", "op"],
        "additionalProperties": false
    })
}

pub(crate) fn job_spec() -> ToolSpec {
    ToolSpec {
        name: "job".into(),
        description: "Read or stop one `shell` job by its job id (`read`, `wait` or \
                      `kill`). Plain `read` returns the output since your last read plus \
                      `[status: ...]` (running / exited n / signal n / killed / timed \
                      out). With `tail_lines` (last N), `from_line`/`max_lines` (numbered \
                      range, page with the `more` hint) or `pattern` (matching lines, Rust \
                      regex) it reads the full file by line instead and names the file's \
                      total line count. `wait` blocks until the job ends or `timeout_ms` \
                      elapses and never kills it. `kill` stops \
                      the job's whole process group (SIGTERM, then SIGKILL after 2 s); a finished \
                      job is a no-op. The output file is outside the workspace, so this \
                      tool is the only way to read it. A completion notice tells you when a \
                      running job ends — prefer it over `wait`; never sleep or poll."
            .into(),
        input_schema: job_schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

/// The `job_id` schema `job` with `op: kill` parses.
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

/// The `job` argument shape for `op: kill` (just the id).

#[derive(Deserialize)]
struct JobIdArgs {
    job_id: String,
}

/// The `job` arguments: which job, and which of the three operations.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum JobOp {
    Read,
    Wait,
    Kill,
}

#[derive(Deserialize)]
struct JobArgs {
    id: String,
    op: JobOp,
    timeout_ms: Option<u64>,
    /// The full-file read arguments ([`crate::output::FileArgs`]); only `read` uses them.
    /// `mode` never survives validation (the schema has no such property).
    #[serde(flatten)]
    file: crate::output::FileArgs,
}

/// `job`: the model's only background tool (ADR 0037 decision 4). `read` without a range is
/// the incremental ring plus `[status]`; with one range form it reads the full file by line;
/// `wait` blocks (never kills); `kill` stops the session (a finished job is a no-op).
/// Parameter errors use [`args::invalid`] like the other tools.
pub(crate) async fn run_job(
    jobs: &Jobs,
    limits: &Limits,
    arguments: &serde_json::Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let args: JobArgs = args::parse("job", &job_schema(), arguments)?;
    match args.op {
        JobOp::Kill => run_kill(jobs, &kill_args(&args.id)),
        JobOp::Wait => {
            if let Some(mode) = crate::output::FileMode::from_args("job", &args.file, arguments)? {
                // A range names a file read even with `op: wait`: the file arguments always
                // worked this way, so an old transcript's `wait: true` + range still reads.
                return read_output_file(jobs, &args.id, mode, limits);
            }
            let timeout = clamp_wait(args.timeout_ms);
            let result = jobs
                .output(&args.id, true, timeout, &cx.cancellation)
                .await?;
            Ok(render_output(&args.id, &result, timeout))
        }
        JobOp::Read => {
            if let Some(mode) = crate::output::FileMode::from_args("job", &args.file, arguments)? {
                return read_output_file(jobs, &args.id, mode, limits);
            }
            let result = jobs.read_increment(&args.id)?;
            Ok(render_output(&args.id, &result, Duration::ZERO))
        }
    }
}

fn kill_args(id: &str) -> serde_json::Value {
    serde_json::json!({ "job_id": id })
}

fn clamp_wait(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_MS).clamp(1, MAX_WAIT_MS))
}

/// Renders an incremental or waited read: the increment, the dropped-bytes note, the
/// timeout/cancel note, then `[status]`.
fn render_output(id: &str, result: &OutputResult, timeout: Duration) -> ToolOutput {
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
    // A wait that timed out in the instant the job settled must not claim it is still running:
    // the state is the authority, the timeout is only why the wait ended.
    if result.waited_out && result.state.is_running() {
        text.push_str(&format!(
            "[still running after {} ms; the job was not killed]\n",
            timeout.as_millis()
        ));
    }
    if result.wait_cancelled {
        text.push_str("[the turn was cancelled; the job keeps running]\n");
    }
    text.push_str(&format!("[status: {}]\n", result.state.label()));

    ToolOutput {
        content: vec![ContentPart::text(text)],
        structured: Some(json!({
            "job_id": id,
            "state": result.state.label(),
            "running": result.state.is_running(),
            "output": result.output,
            "new_bytes": result.increment.len(),
            "dropped_bytes": result.dropped,
        })),
    }
}

/// Reads a job's own output file in the requested [`crate::output::FileMode`]
/// (ADR 0039 decision 1: the model reads the whole output through the tools). The
/// id must be a real registry key; a path-like id is an unknown job, never a path.
pub(crate) fn read_output_file(
    jobs: &Jobs,
    id: &str,
    mode: crate::output::FileMode,
    limits: &Limits,
) -> Result<ToolOutput, ToolFailure> {
    let cell = jobs.get(id)?;
    let path = lock(&cell).output.clone();
    let file = crate::output::open_output_read(&path)
        .map_err(|e| crate::output::read_failure("jobs_output", &e))?;
    crate::output::read_file(
        &file,
        "job_id",
        id,
        mode,
        limits.default_read_bytes,
        limits.default_grep_matches,
        "jobs_output",
    )
}

/// Stops a job (`job` with `op: kill`): SIGTERM the session, SIGKILL after the grace.
/// A settled job is a no-op.
pub(crate) fn run_kill(
    jobs: &Jobs,
    arguments: &serde_json::Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: JobIdArgs = args::parse("job", &kill_schema(), arguments)?;
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

    /// A job cell as `start`/`adopt` build it, for the settle test below.
    fn test_cell() -> Arc<Mutex<JobCell>> {
        let (settled, _) = tokio::sync::watch::channel(false);
        Arc::new(Mutex::new(JobCell {
            command: "test".into(),
            started: Instant::now(),
            state: JobState::Running,
            retained: false,
            termination: None,
            ring: Ring::default(),
            read_offset: 0,
            output: PathBuf::from("test.out"),
            sid: 0,
            waiters: 0,
            reaped: Arc::new(Mutex::new(false)),
            settled,
        }))
    }

    /// The root cause of a wait on a settled job sleeping to its timeout: tokio's `send` returns
    /// `Err` **without storing** the value when the channel has no receiver, and `start`/`adopt`
    /// drop the receiver from `watch::channel(false)` at once. A settle with no waiter must store
    /// `true` anyway (`send_replace`), so a reader that subscribes afterwards sees it.
    #[test]
    fn a_settle_with_no_waiter_still_stores_the_watched_value() {
        let jobs = Jobs::new(std::env::temp_dir().join("radian-jobs-settle-test"));
        let cell = test_cell();
        settle(&jobs.inner, &cell, "job-0-0".into(), None);

        assert!(!lock(&cell).state.is_running());
        let mut rx = lock(&cell).settled.subscribe();
        assert!(
            *rx.borrow_and_update(),
            "a reader that subscribes after the settle must see it"
        );
    }
}
