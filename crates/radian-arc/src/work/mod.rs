//! `radian arc work`: the trusted wrapper around one worker attempt (ADR 0048 decision 7).
//!
//! This is an **internal** command: arc runs it, a person does not. It is hidden from
//! `--help` and is not part of the user manual (AGENTS.md: an input that exists only for
//! an internal child process is not documented). It takes everything it needs as flags,
//! because nothing else of arc exists yet: no `init`, no backlog, no ledger, no
//! scheduling loop.
//!
//! One run does, in order:
//!
//! 1. create or reuse the linked worktree `<repository>/.radian/arc/worktrees/<task>` on
//!    branch `work/<task>`, and take a flock on that directory (two attempts of one task
//!    never run together); a first attempt fetches the configured remote's main branch and
//!    starts from the fetched tip;
//! 2. write the host-local attempt record under `<repository>/.radian/arc/attempts/`;
//! 3. run `radian exec -p - --session-id <task> --workspace <worktree> --profile workspace
//!    --yes --output jsonl` (with `--resume <task>` when the task's session already
//!    exists, and the caller's extra arguments), the brief on its stdin, stdout into
//!    `runs/…/result.jsonl`;
//! 4. while the child runs, enforce the attempt's limits here: a stall, the wall time, or a
//!    `cancel` records `signal.json`, sends SIGTERM to the child's process group and then
//!    SIGKILL after the grace period. These limits belong to this process, so they hold after
//!    the `run` process that started it exits or is killed (decision 7.8).
//!    A heartbeat thread writes `attempts/<task>/<attempt>/heartbeat.json` for a later pass
//!    to judge liveness (decision 7.9);
//! 5. after the child exits, read the `Result` line and the exit code, read `head_sha`,
//!    list uncommitted and untracked files, and run the delivery checks;
//! 6. push `work/<task>` only when the outcome is `completed` and every check passed, then
//!    verify the remote ref with `ls-remote`;
//! 7. print one `AttemptFinished` JSON line as its last stdout line and exit with the
//!    child's exit code (or 1 when a check failed after a `completed` child).
//!
//! Every git command this module runs goes through the crate's one git helper
//! (`crate::git`): built from the shared host builder, an empty hooks path, and
//! `GIT_CONFIG_NOSYSTEM=1`, so no file the model can write changes what the wrapper runs
//! (ADR 0048 decision 7.3). The push credential stays in this process's environment; in v0
//! the wrapper and the model are the same OS user, so that is not isolation.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{Value, json};

use crate::git::GitOutput;
use crate::process;
use crate::store::attempt_ops::{HEARTBEAT_FILE, SIGNAL_FILE, heartbeat_interval};

/// How often the wrapper looks at the worker, its jsonl stream and a cancel signal.
const POLL: Duration = Duration::from_millis(100);

/// The `radian arc work` flags. `radian-tui`'s `arc work` subcommand fills this.
pub struct Options {
    /// The task id: the session id, the branch suffix and the worktree name.
    pub task: String,
    /// The attempt number, 1-based.
    pub attempt: u32,
    /// The random token the scheduling loop expects back in `AttemptFinished`.
    pub token: String,
    /// A later attempt: continue the task's session (`--resume <task>`) when it exists. An
    /// earlier attempt that failed before the model started left no session, and this one
    /// then starts it with `--session-id <task>` (ADR 0048 decision 7.2).
    pub resume: bool,
    /// Let a branch with no change past `base_sha` pass the delivery check (ADR 0048
    /// decision 8: a brief can allow a no-op).
    pub allow_noop: bool,
    /// The remote the wrapper fetches the main branch from and pushes `work/<task>` to.
    /// Internal: `arc` sets it from the repository configuration (default `origin`); a
    /// person does not.
    pub remote: String,
    /// The main branch a first attempt starts from. Internal: `arc` sets it (default
    /// `main`); a person does not.
    pub main_branch: String,
    /// Extra `radian exec` arguments after `--` (the model route in tests: `--model`,
    /// `--base-url`).
    pub extra_args: Vec<String>,
    /// Internal: the attempt's stall limit in seconds; `0` means no limit. arc passes the
    /// task's `stall_timeout_secs`, else `[limits] stall_timeout_secs`. The wrapper enforces
    /// it, so the limit survives the `run` process that started it (ADR 0048 decision 7.8).
    pub stall_timeout_secs: u64,
    /// Internal: the attempt's absolute wall-time limit in seconds; `0` means no limit.
    pub max_wall_time_secs: u64,
    /// Internal: how long a signalled worker gets before SIGKILL (`[limits]
    /// cancel_grace_secs`).
    pub cancel_grace_secs: u64,
    /// Test-only: run this shell command as the worker instead of `radian exec`. It exists so
    /// the forwarding tests can start a plain process that spawns a child of its own, which
    /// a real `radian exec` does not (its tools run in their own sessions). Never set by
    /// arc; an input that exists only for tests is not documented (AGENTS.md).
    pub worker_command: Option<String>,
}

/// Exit codes (ADR 0048 decision 1): 0 success, 1 error, 2 usage, 4 a lock is held.
const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_LOCKED: i32 = 4;

/// The arc directory's entries the worker must not read (ADR 0048 decision 5): every entry
/// except `worktrees/`, because the worker's own workspace lives there. The list is fixed, so
/// an entry is denied before it exists; any other entry that does exist is added by
/// [`deny_paths`].
const DENIED_ENTRIES: &[&str] = &[
    "config.toml",
    "plans",
    "tasks",
    "ledger",
    "runs",
    "attempts",
    "gates",
    "state.lock",
    "runner.lock",
];

/// The worker's pid, while a child runs. The SIGTERM handler reads it to forward the signal.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// Whether a SIGTERM arrived before the child existed (or after it exited).
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The stop handler (SIGTERM and SIGINT): remember the request and forward it to the worker's
/// whole process group. Both the atomic stores and `killpg(2)` are async-signal-safe.
extern "C" fn forward_stop(_signal: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        process::signal_group(pid as u32, process::SIGTERM);
    }
}

fn install_stop_handlers() {
    // SAFETY: the handler only touches atomics and calls `kill(2)`; both are
    // async-signal-safe. `signal` with a positive handler installs it for the process. SIGINT
    // is treated exactly like SIGTERM: a Ctrl-C sent to the wrapper alone still forwards to
    // the worker and ends the attempt with its record.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            libc::signal(signal, forward_stop as *const () as libc::sighandler_t);
        }
    }
}

/// Runs one attempt and returns the process exit code.
pub fn run(options: Options) -> i32 {
    match run_inner(&options) {
        Ok(code) => code,
        Err(failure) => {
            eprintln!("radian arc work: {}", failure.message);
            failure.code
        }
    }
}

/// A wrapper-level failure: the message goes to stderr and the code is the exit code.
struct Failure {
    message: String,
    code: i32,
    /// The attempt's wall clock ran out; the caller records the attempt as `wall_time`.
    wall: bool,
}

impl Failure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_ERROR,
            wall: false,
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_USAGE,
            wall: false,
        }
    }

    fn wall() -> Self {
        Self {
            message: "max_wall_time_secs reached".to_owned(),
            code: EXIT_ERROR,
            wall: true,
        }
    }
}

fn run_inner(o: &Options) -> Result<i32, Failure> {
    // The task id names a directory and a branch, so it is checked before anything is
    // created. Every other check runs once the attempt directories exist, so a refused
    // argument still leaves the record the next pass applies (ADR 0048 decision 9).
    validate_task(o)?;
    // ADR 0048 decision 9: arc cancels an attempt with SIGTERM and expects the wrapper to
    // pass it on, wait for the worker, and still write `AttemptFinished`. Without a handler
    // the wrapper would die at once and leave no record for arc to apply.
    install_stop_handlers();
    // The wall clock starts here and covers everything this process does for the attempt: the
    // worktree and its fetch, the model, the delivery checks and the push (ADR 0048
    // decisions 7.8 and 9). Every wrapper git command gets the time that is left.
    let started = Instant::now();
    let repo = repo_root()?;
    let arc = repo.join(".radian").join("arc");
    let worktree = arc.join("worktrees").join(&o.task);
    let branch = format!("work/{}", o.task);

    let runs = arc
        .join("runs")
        .join(&o.task)
        .join("attempts")
        .join(format!("{}-{}", o.task, o.attempt));
    let attempt_dir = arc
        .join("attempts")
        .join(&o.task)
        .join(o.attempt.to_string());
    for dir in [&runs, &attempt_dir] {
        fs::create_dir_all(dir)
            .map_err(|error| Failure::error(format!("cannot create {}: {error}", dir.display())))?;
    }
    let wrapper_pid = std::process::id();
    let signal = runs.join(SIGNAL_FILE);
    let setup = Setup {
        repo,
        arc,
        worktree,
        branch,
        runs,
        attempt_dir,
        wrapper_pid,
        clock: Clock::new(started, Duration::from_secs(o.max_wall_time_secs), signal),
    };
    // From here on every failure writes the attempt's finish record before the wrapper exits:
    // the next pass applies it and frees the attempt's `[limits] concurrency` slot at once
    // (ADR 0048 decision 9). Only a failure before the two directories exist (a task id that
    // names no path, a directory that cannot be created) has no record to leave; reconcile
    // then applies the attempt after the stall window.
    attempt(o, &setup).map_err(|failure| record_failure(o, &setup, failure))
}

/// The rest of the attempt: the remaining argument checks, the heartbeat, and the work itself.
/// The heartbeat starts with the attempt directory and lives until both end records are
/// written: the worktree setup, the model wait and the delivery and push checks all count as
/// alive (ADR 0048 decisions 7.9 and 9).
fn attempt(o: &Options, s: &Setup) -> Result<i32, Failure> {
    validate_rest(o)?;
    let mut heartbeat = Heartbeat::start(
        &s.attempt_dir,
        Beat {
            attempt_id: format!("{}-{}", o.task, o.attempt),
            pid: None,
            process_started_at: None,
            wrapper_pid: s.wrapper_pid,
            wrapper_started_at: process::start_time(s.wrapper_pid),
        },
        o.stall_timeout_secs,
    )?;
    let result = match run_attempt(o, s, &heartbeat) {
        Ok(code) => Ok(code),
        // The wall clock ran out (a git command was killed or a later step saw it expired):
        // the attempt still ends with its records and the `wall_time` class.
        Err(failure) if failure.wall => finish_wall(o, s),
        Err(failure) => Err(failure),
    };
    heartbeat.stop();
    result
}

/// Writes the attempt's finish record for a failure and returns the failure unchanged. The
/// record is what the next pass applies, so a wrapper that fails at once (a refused argument,
/// a held worktree lock) frees its concurrency slot in that pass instead of after the stall
/// window (ADR 0048 decision 9). A failure to record it never hides the failure itself.
fn record_failure(o: &Options, s: &Setup, failure: Failure) -> Failure {
    let base_sha = recorded_base_sha(&s.arc, &o.task);
    let _ = write_finish(
        o,
        s,
        Finish {
            base_sha: &base_sha,
            head_sha: None,
            dirty: Vec::new(),
            pushed: false,
            remote_sha: None,
            outcome: "terminated",
            exit_code: failure.code,
            failure: Some(failure.message.clone()),
        },
    );
    failure
}

/// The base commit a first attempt recorded, when it did: the early finish records name it.
fn recorded_base_sha(arc: &Path, task: &str) -> String {
    fs::read_to_string(arc.join("attempts").join(task).join("base_sha"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

/// The paths one attempt works in, and its wall clock.
struct Setup {
    repo: PathBuf,
    arc: PathBuf,
    worktree: PathBuf,
    branch: String,
    runs: PathBuf,
    attempt_dir: PathBuf,
    wrapper_pid: u32,
    clock: Clock,
}

/// The attempt's wall clock. `deadline` is `None` when `max_wall_time_secs` is 0.
struct Clock {
    deadline: Option<Instant>,
    signal: PathBuf,
}

impl Clock {
    fn new(started: Instant, wall: Duration, signal: PathBuf) -> Self {
        Self {
            deadline: (!wall.is_zero()).then(|| started + wall),
            signal,
        }
    }

    /// The time left, or `None` when the attempt has no wall limit.
    fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    fn expired(&self) -> bool {
        self.remaining()
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// Records that the attempt ran out of wall time. The first signal wins, so a cancel or a
    /// stall that was recorded first stays.
    fn mark(&self) -> Result<(), Failure> {
        write_signal(&self.signal, "wall_time")
    }

    fn deadline(&self) -> Option<Instant> {
        self.deadline
    }
}

/// The `runs/<task>/attempts/<task>-<n>` path of one attempt.
fn run_attempt(o: &Options, s: &Setup, heartbeat: &Heartbeat) -> Result<i32, Failure> {
    let created = ensure_worktree(
        &s.repo,
        &s.worktree,
        &s.branch,
        &o.remote,
        &o.main_branch,
        &s.clock,
    )?;
    // Held until this function returns: one attempt of one task at a time.
    let _lock = lock_worktree(&s.worktree)?;

    let result_path = s.runs.join("result.jsonl");
    let stderr_path = s.runs.join("stderr.log");
    let brief = read_stdin()?;
    let base_sha = base_sha(&s.repo, &s.arc, o, &s.branch, created, &s.clock)?;
    // The moved-ref guard: a later attempt may only run when `work/<task>` still points at
    // the tip the previous attempt ended on. Another worker can move the ref from inside its
    // `workspace` sandbox (the common git directory's refs are writable, ADR 0038), and with
    // a matching tree every delivery check would pass; so this check runs before the child,
    // and nothing is recorded for the next attempt when it fails.
    if let Some(last) = last_head(&s.arc, &o.task) {
        match check_start(&s.repo, &s.worktree, &s.branch, &last, &s.clock) {
            Ok(()) => {}
            Err(failure) if failure.wall => return Err(failure),
            Err(failure) => return refuse(o, s, &base_sha, &failure.message),
        }
    }
    // The profile's steps before an attempt: a reused worktree merges the remote main first
    // (ADR 0048 decision 7). The brief the worker gets may start with a conflict section; the
    // task file's brief is never changed.
    let brief = before_attempt(o, s, created, brief)?;
    let start_sha = branch_tip(&s.repo, &s.branch, &s.clock)?.ok_or_else(|| {
        Failure::error(format!("cannot read {} in {}", s.branch, s.repo.display()))
    })?;
    let started_at = now();
    let deny = deny_paths(&s.arc);
    let mut child = spawn_child(o, &s.worktree, &deny, &result_path, &stderr_path, &brief)?;
    let pid = child.id();
    CHILD_PID.store(pid as i32, Ordering::SeqCst);
    // A SIGTERM that arrived during the worktree setup is forwarded as soon as there is a
    // child to forward it to.
    if STOP_REQUESTED.load(Ordering::SeqCst) {
        process::signal_group(pid, process::SIGTERM);
    }
    let processor_started_at = process_start(pid).unwrap_or_else(|| started_at.clone());
    let wrapper_started_at = process::start_time(s.wrapper_pid);
    heartbeat.set_worker(pid, &processor_started_at);
    write_json(
        &s.attempt_dir.join("identity.json"),
        &json!({
            "attempt_id": format!("{}-{}", o.task, o.attempt),
            "token": o.token,
            "pid": pid,
            "process_started_at": processor_started_at,
            // The scheduling side signals the wrapper, not the worker, so the attempt
            // record carries the wrapper's own identity too (decision 9).
            "wrapper_pid": s.wrapper_pid,
            "wrapper_process_started_at": wrapper_started_at,
            "session_id": o.task,
            "worktree": s.worktree.display().to_string(),
            "branch": s.branch,
            "started_at": started_at,
        }),
    )?;
    // The limits belong to this process, not to the scheduling loop that started it: a
    // one-shot `run`, or a killed one, leaves the worker bounded (decision 7.8).
    let status = monitor_child(
        &mut child,
        &result_path,
        &s.runs.join(SIGNAL_FILE),
        &Limits::of(o),
        s.clock.deadline(),
    )?;
    CHILD_PID.store(0, Ordering::SeqCst);
    let ended_at = now();
    let exit_code = exit_code_of(&status);

    let result = last_result(&result_path);
    let outcome = result
        .as_ref()
        .and_then(|value| value.get("outcome"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "terminated".to_owned());
    let head_sha = match git(&s.worktree, &["rev-parse", "HEAD"], &s.clock) {
        Ok(sha) => Some(sha.trim().to_owned()).filter(|sha| !sha.is_empty()),
        Err(failure) if failure.wall => return Err(failure),
        Err(_) => None,
    };
    let (dirty, status_paths) = match dirty_paths(&s.worktree, &s.clock) {
        Ok(paths) => (paths.clone(), Ok(paths)),
        Err(failure) if failure.wall => return Err(failure),
        Err(failure) => (Vec::new(), Err(failure)),
    };
    let main_ref = format!("refs/remotes/{}/{}", o.remote, o.main_branch);
    write_json(
        &s.runs.join("execution.json"),
        &json!({
            "attempt_id": format!("{}-{}", o.task, o.attempt),
            "host": hostname(),
            "token": o.token,
            "pid": pid,
            "process_started_at": processor_started_at,
            "wrapper_pid": s.wrapper_pid,
            "session_id": o.task,
            "worktree": s.worktree.display().to_string(),
            "branch": s.branch,
            "base_sha": base_sha,
            "head_sha": head_sha,
            "started_at": started_at,
            "ended_at": ended_at,
        }),
    )?;

    // The wrapper, not the model, decides whether the attempt delivered anything (ADR 0048
    // decisions 7 and 8). Only a `completed` child with every check passed is pushed.
    let mut failure = None;
    let mut pushed = false;
    let mut remote_sha: Option<String> = None;
    if outcome != "completed" {
        failure = Some(format!("outcome {outcome}"));
    } else {
        let check = check_delivery(
            &Delivery {
                repo: &s.repo,
                worktree: &s.worktree,
                branch: &s.branch,
                base: &base_sha,
                start: &start_sha,
                main: &main_ref,
                head: head_sha.as_deref(),
                status_paths: &status_paths,
                allow_noop: o.allow_noop,
            },
            &s.clock,
        );
        match check {
            Err(problem) if problem.wall => return Err(problem),
            Err(problem) => failure = Some(problem.message),
            Ok(()) => {
                let head = head_sha.as_deref().unwrap_or_default();
                match push(&s.repo, &o.remote, &s.branch, &s.clock) {
                    Err(problem) if problem.wall => return Err(problem),
                    Err(problem) => failure = Some(problem.message),
                    Ok(()) => {
                        pushed = true;
                        match remote_sha_of(&s.repo, &o.remote, &s.branch, &s.clock) {
                            Err(problem) if problem.wall => return Err(problem),
                            Err(problem) => failure = Some(problem.message),
                            Ok(remote) => {
                                if remote != head {
                                    failure =
                                        Some(format!("remote ref is {remote}, expected {head}"));
                                }
                                remote_sha = Some(remote);
                            }
                        }
                    }
                }
            }
        }
    }

    // Every attempt that ran records the branch tip it ended on (host-local): a later attempt
    // must still find that tip on `work/<task>` before it starts (the guard above). A tip
    // that cannot be read (the child removed the branch) leaves the previous record in
    // place, so the next attempt refuses instead of running on a branch it cannot trust.
    if let Some(tip) = branch_tip(&s.repo, &s.branch, &s.clock)? {
        write_text(&last_head_path(&s.arc, &o.task), &tip)?;
    }
    write_finish(
        o,
        s,
        Finish {
            base_sha: &base_sha,
            head_sha,
            dirty,
            pushed,
            remote_sha,
            outcome: &outcome,
            exit_code,
            failure,
        },
    )
}

/// One `AttemptFinished` record, written to both places arc reads it from.
struct Finish<'a> {
    base_sha: &'a str,
    head_sha: Option<String>,
    dirty: Vec<String>,
    pushed: bool,
    remote_sha: Option<String>,
    outcome: &'a str,
    exit_code: i32,
    failure: Option<String>,
}

fn write_finish(o: &Options, s: &Setup, finish: Finish<'_>) -> Result<i32, Failure> {
    let attempt_failed = finish.failure.is_some();
    let record = json!({
        "type": "AttemptFinished",
        "attempt_id": format!("{}-{}", o.task, o.attempt),
        "token": o.token,
        "exit_code": finish.exit_code,
        "outcome": finish.outcome,
        "base_sha": finish.base_sha,
        "head_sha": finish.head_sha,
        "dirty": finish.dirty,
        "pushed": finish.pushed,
        "remote_sha": finish.remote_sha,
        "failure": finish.failure,
    });
    let line = serde_json::to_string(&record)
        .map_err(|error| Failure::error(format!("cannot serialize AttemptFinished: {error}")))?;
    write_text(&s.runs.join("attempt_finished.json"), &line)?;
    write_text(&s.attempt_dir.join("status.json"), &line)?;
    println!("{line}");

    // ADR 0044 section 3: the child's exit code passes through. A check that failed after a
    // `completed` (exit 0) child is this wrapper's own failure: exit 1.
    Ok(if attempt_failed && finish.exit_code == 0 {
        EXIT_ERROR
    } else {
        finish.exit_code
    })
}

/// The wall clock ran out: record `wall_time`, keep the branch tip from the loose ref (no
/// further git call), and write the attempt's records. The retry rules then apply.
fn finish_wall(o: &Options, s: &Setup) -> Result<i32, Failure> {
    s.clock.mark()?;
    if let Some(tip) = loose_branch_tip(&s.repo, &s.branch) {
        let _ = write_text(&last_head_path(&s.arc, &o.task), &tip);
    }
    let base_sha = recorded_base_sha(&s.arc, &o.task);
    write_finish(
        o,
        s,
        Finish {
            base_sha: &base_sha,
            head_sha: None,
            dirty: Vec::new(),
            pushed: false,
            remote_sha: None,
            outcome: "terminated",
            exit_code: EXIT_ERROR,
            failure: Some("max_wall_time_secs reached".to_owned()),
        },
    )
}

/// The branch tip from its loose ref file, without running git. A commit writes the loose
/// ref; `git gc` would pack it, and a missing file just leaves the previous record.
fn loose_branch_tip(repo: &Path, branch: &str) -> Option<String> {
    let text =
        fs::read_to_string(repo.join(".git").join("refs").join("heads").join(branch)).ok()?;
    let sha = text.trim();
    (!sha.is_empty()).then(|| sha.to_owned())
}

/// What the heartbeat thread writes: the attempt's identity. The worker's fields are filled
/// in once it is spawned.
#[derive(Clone)]
struct Beat {
    attempt_id: String,
    pid: Option<u32>,
    process_started_at: Option<String>,
    wrapper_pid: u32,
    wrapper_started_at: Option<String>,
}

/// The wrapper's heartbeat writer (ADR 0048 decision 7.9). One beat is written at once; the
/// thread then refreshes `last_beat` every interval until [`Heartbeat::stop`] (or, on an
/// early return, until the guard drops). A rename over the old file means a reader never sees
/// half a record.
struct Heartbeat {
    stop: Arc<AtomicBool>,
    beat: Arc<std::sync::Mutex<Beat>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Heartbeat {
    fn start(dir: &Path, beat: Beat, stall_secs: u64) -> Result<Self, Failure> {
        let path = dir.join(HEARTBEAT_FILE);
        let beat = Arc::new(std::sync::Mutex::new(beat));
        write_beat(&path, &beat)?;
        let interval = heartbeat_interval(stall_secs);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let shared = Arc::clone(&beat);
        let handle = std::thread::spawn(move || {
            loop {
                let deadline = Instant::now() + interval;
                while Instant::now() < deadline {
                    if flag.load(Ordering::SeqCst) {
                        return;
                    }
                    std::thread::sleep(POLL);
                }
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                // A heartbeat that cannot be written is not the attempt's failure: the
                // next pass then judges the attempt by the age of the last beat it has.
                let _ = write_beat(&path, &shared);
            }
        });
        Ok(Self {
            stop,
            beat,
            handle: Some(handle),
        })
    }

    /// Adds the worker's identity to every beat from now on.
    fn set_worker(&self, pid: u32, process_started_at: &str) {
        if let Ok(mut beat) = self.beat.lock() {
            beat.pid = Some(pid);
            beat.process_started_at = Some(process_started_at.to_owned());
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        // A start refusal or an error returns before [`Heartbeat::stop`]; the beats still end.
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn write_beat(path: &Path, beat: &std::sync::Mutex<Beat>) -> Result<(), Failure> {
    let beat = beat
        .lock()
        .map_err(|_| Failure::error("the heartbeat record was poisoned"))?;
    let record = json!({
        "attempt_id": beat.attempt_id,
        "pid": beat.pid,
        "process_started_at": beat.process_started_at,
        "wrapper_pid": beat.wrapper_pid,
        "wrapper_process_started_at": beat.wrapper_started_at,
        "last_beat": now(),
    });
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| Failure::error(format!("cannot encode the heartbeat: {error}")))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, format!("{text}\n")).map_err(|error| {
        Failure::error(format!("cannot write {}: {error}", temporary.display()))
    })?;
    fs::rename(&temporary, path)
        .map_err(|error| Failure::error(format!("cannot write {}: {error}", path.display())))
}

/// One attempt's limits, as the wrapper enforces them (ADR 0048 decisions 7.8 and 9). The
/// wall clock lives in [`Clock`], because it also bounds the wrapper's git commands.
struct Limits {
    stall: Duration,
    grace: Duration,
}

impl Limits {
    fn of(o: &Options) -> Self {
        Self {
            stall: Duration::from_secs(o.stall_timeout_secs),
            grace: Duration::from_secs(o.cancel_grace_secs),
        }
    }
}

/// Waits for the worker and stops it with SIGTERM (then SIGKILL after the grace period) when
/// it stalls, runs past the attempt's wall deadline, or a `cancel` recorded a signal. The
/// scheduling loop does not watch the attempt any more: this process owns the limits.
fn monitor_child(
    child: &mut Child,
    result_path: &Path,
    signal_path: &Path,
    limits: &Limits,
    wall_deadline: Option<Instant>,
) -> Result<ExitStatus, Failure> {
    let mut last_growth = Instant::now();
    let mut last_len = file_len(result_path);
    let mut signalled = false;
    let mut kill_at: Option<Instant> = None;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| Failure::error(format!("cannot wait for the worker: {error}")))?
        {
            return Ok(status);
        }
        let now = Instant::now();
        let len = file_len(result_path);
        if len != last_len {
            last_len = len;
            last_growth = now;
        }
        if !signalled {
            // The order of the old monitor: a cancel from another process first, then the
            // wall clock, then the stall timer.
            let reason = if signal_path.is_file() {
                // `radian arc cancel` wrote its own reason; this process adds none.
                Some("cancel")
            } else if wall_deadline.is_some_and(|deadline| now >= deadline) {
                Some("wall_time")
            } else if !limits.stall.is_zero() && now.duration_since(last_growth) >= limits.stall {
                Some("stall")
            } else {
                None
            };
            if let Some(reason) = reason {
                write_signal(signal_path, reason)?;
                process::signal_group(child.id(), process::SIGTERM);
                signalled = true;
                kill_at = Some(now + limits.grace);
            }
        }
        if let Some(at) = kill_at
            && now >= at
        {
            process::signal_group(child.id(), process::SIGKILL);
            kill_at = None;
        }
        std::thread::sleep(POLL);
    }
}

fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

/// Records why the wrapper was signalled. The first reason wins, exactly as the store's
/// `write_signal` does: a stall followed by a cancel records the stall.
fn write_signal(path: &Path, reason: &str) -> Result<(), Failure> {
    if path.is_file() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .map_err(|error| Failure::error(format!("cannot create {}: {error}", dir.display())))?;
    }
    let text = serde_json::to_string_pretty(&json!({ "reason": reason }))
        .map_err(|error| Failure::error(format!("cannot encode the signal: {error}")))?;
    fs::write(path, format!("{text}\n"))
        .map_err(|error| Failure::error(format!("cannot write {}: {error}", path.display())))
}

/// The task id is a file name and a branch suffix: keep it to characters that are safe in
/// both, so a caller cannot walk out of the arc directory or name an option. It is checked
/// before the wrapper creates anything; the rest of the flags are checked by
/// [`validate_rest`], after the attempt directories exist.
fn validate_task(o: &Options) -> Result<(), Failure> {
    if o.task.is_empty() || o.task.len() > 64 {
        return Err(Failure::usage("--task needs 1 to 64 characters"));
    }
    if o.task == "."
        || o.task == ".."
        || !o
            .task
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Failure::usage(
            "--task takes ASCII letters, digits, `-`, `_` and `.` only",
        ));
    }
    Ok(())
}

/// The other flags. They are checked after the attempt directory exists, so a refused value
/// (example: a remote name from `config.toml` with a character a ref cannot hold) still
/// leaves the `AttemptFinished` record the next pass applies (ADR 0048 decision 9).
fn validate_rest(o: &Options) -> Result<(), Failure> {
    if o.attempt == 0 {
        return Err(Failure::usage("--attempt is 1-based"));
    }
    if o.token.is_empty() {
        return Err(Failure::usage("--token needs a value"));
    }
    // The internal remote and main-branch names end up as git arguments, so keep them to
    // characters a ref name uses and refuse a leading `-` (an option in disguise).
    for (flag, value) in [("--remote", &o.remote), ("--main-branch", &o.main_branch)] {
        if value.is_empty()
            || value.starts_with('-')
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
        {
            return Err(Failure::usage(format!(
                "{flag} takes a name of ASCII letters, digits, `-`, `_`, `.` and `/`"
            )));
        }
    }
    Ok(())
}

/// The repository root, found from the current directory. The wrapper runs from the main
/// checkout: a linked worktree's `.git` is a file, and its worktrees are not the place to
/// create another one.
fn repo_root() -> Result<PathBuf, Failure> {
    let cwd = std::env::current_dir()
        .map_err(|error| Failure::error(format!("cannot read the current directory: {error}")))?;
    // Local `rev-parse`, before the attempt's clock exists; it names no remote and cannot
    // wait on one.
    let out = crate::git::run(&cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| Failure::error(format!("{} is not inside a git repository", cwd.display())))?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "{} is not inside a git repository",
            cwd.display()
        )));
    }
    let root = PathBuf::from(out.stdout.trim());
    if root.join(".git").is_file() {
        return Err(Failure::error(format!(
            "{} is a linked worktree; run `radian arc work` from the main checkout",
            root.display()
        )));
    }
    if !root.join(".git").is_dir() {
        return Err(Failure::error(format!(
            "{} has no .git directory",
            root.display()
        )));
    }
    Ok(root)
}

/// Creates the worktree from the configured remote's main branch when it is missing, or
/// reuses it. Returns whether this run created it (the first attempt records `base_sha` from
/// the fetched main branch and starts there).
fn ensure_worktree(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    remote: &str,
    main_branch: &str,
    clock: &Clock,
) -> Result<bool, Failure> {
    let parent = worktree
        .parent()
        .ok_or_else(|| Failure::error(format!("{} has no directory", worktree.display())))?;
    fs::create_dir_all(parent)
        .map_err(|error| Failure::error(format!("cannot create {}: {error}", parent.display())))?;
    // Attempts that start together (`[limits] concurrency` above one) must not run
    // `git worktree add` at the same time: git writes the branch's configuration, and two
    // writers collide on `.git/config.lock` and make the second attempt fail. The
    // `worktrees/` directory's own flock makes creation (and the fetch behind it) one at a
    // time; the guard lives until this function returns, before the worker starts.
    let _setup = File::open(parent).map_err(|error| {
        Failure::error(format!(
            "cannot open {} for the setup lock: {error}",
            parent.display()
        ))
    })?;
    _setup.lock_exclusive().map_err(|error| {
        Failure::error(format!(
            "cannot lock {} for the setup: {error}",
            parent.display()
        ))
    })?;
    if worktree.join(".git").exists() {
        let top = match git(worktree, &["rev-parse", "--show-toplevel"], clock) {
            Ok(top) => top,
            Err(failure) if failure.wall => return Err(failure),
            Err(failure) => {
                return Err(Failure::error(format!(
                    "{} exists but is not a usable worktree: {}",
                    worktree.display(),
                    failure.message
                )));
            }
        };
        if !same_path(Path::new(top.trim()), worktree) {
            return Err(Failure::error(format!(
                "{} is not the worktree of {}",
                worktree.display(),
                repo.display()
            )));
        }
        let common = match git(worktree, &["rev-parse", "--git-common-dir"], clock) {
            Ok(common) => common,
            Err(failure) if failure.wall => return Err(failure),
            Err(failure) => {
                return Err(Failure::error(format!(
                    "cannot read the worktree's common git directory: {}",
                    failure.message
                )));
            }
        };
        if !same_path(Path::new(common.trim()), &repo.join(".git")) {
            return Err(Failure::error(format!(
                "{} does not belong to {}",
                worktree.display(),
                repo.display()
            )));
        }
        return Ok(false);
    }
    if worktree.exists() {
        return Err(Failure::error(format!(
            "{} exists and is not a linked worktree",
            worktree.display()
        )));
    }
    let path = worktree.to_string_lossy().into_owned();
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = git_raw(
        repo,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
        clock,
    )?
    .status
    .success();
    if branch_exists {
        git(repo, &["worktree", "add", &path, branch], clock)?;
    } else {
        // The first attempt starts from the current remote main, not from whatever this
        // clone last fetched. The refspec names the tracking ref explicitly so a remote
        // with a different fetch configuration cannot leave it stale.
        let start = format!("{remote}/{main_branch}");
        let refspec = format!("+refs/heads/{main_branch}:refs/remotes/{start}");
        match git(repo, &["fetch", remote, &refspec], clock) {
            Ok(_) => {}
            Err(failure) if failure.wall => return Err(failure),
            Err(failure) => {
                return Err(Failure::error(format!(
                    "cannot fetch {remote} {main_branch}: {}",
                    failure.message
                )));
            }
        }
        if !git_raw(repo, &["rev-parse", "--verify", "--quiet", &start], clock)?
            .status
            .success()
        {
            return Err(Failure::error(format!(
                "{start} does not exist; a first attempt starts from the remote's main branch"
            )));
        }
        git(
            repo,
            &["worktree", "add", "-b", branch, &path, &start],
            clock,
        )?;
    }
    Ok(true)
}

/// The task's base commit: the fetched remote main branch when this run created the
/// worktree, else the value the first attempt recorded. A worktree made outside the wrapper
/// falls back to the merge-base with the configured remote's main branch.
fn base_sha(
    repo: &Path,
    arc: &Path,
    o: &Options,
    branch: &str,
    created: bool,
    clock: &Clock,
) -> Result<String, Failure> {
    let main = format!("{}/{}", o.remote, o.main_branch);
    let dir = arc.join("attempts").join(&o.task);
    fs::create_dir_all(&dir)
        .map_err(|error| Failure::error(format!("cannot create {}: {error}", dir.display())))?;
    let path = dir.join("base_sha");
    if let Ok(text) = fs::read_to_string(&path) {
        let text = text.trim();
        if !text.is_empty() {
            return Ok(text.to_owned());
        }
    }
    let base = if created {
        git(repo, &["rev-parse", &main], clock)?
    } else {
        git(repo, &["merge-base", &main, branch], clock)?
    };
    let base = base.trim().to_owned();
    write_text(&path, &base)?;
    Ok(base)
}

/// The branch tip the previous attempt ended on, if one is recorded.
fn last_head(arc: &Path, task: &str) -> Option<String> {
    let text = fs::read_to_string(last_head_path(arc, task)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn last_head_path(arc: &Path, task: &str) -> PathBuf {
    arc.join("attempts").join(task).join("last_head")
}

/// The start check: before a reused worktree's child runs, `work/<task>` must still point at
/// the tip the previous attempt recorded, and the worktree HEAD must still be on that
/// branch. Anything else means the ref moved outside this task, and this attempt does not
/// run. A failure with `wall` set is the clock, not a moved ref.
fn check_start(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    last: &str,
    clock: &Clock,
) -> Result<(), Failure> {
    let reference = format!("refs/heads/{branch}");
    let tip = git_raw(
        repo,
        &["rev-parse", "--verify", "--quiet", &reference],
        clock,
    )?;
    if !tip.status.success() || tip.stdout.trim() != last {
        return Err(Failure::error(moved_outside(branch)));
    }
    let head = git_raw(worktree, &["symbolic-ref", "--short", "HEAD"], clock)?;
    if !head.status.success() || head.stdout.trim() != branch {
        return Err(Failure::error(moved_outside(branch)));
    }
    Ok(())
}

fn moved_outside(branch: &str) -> String {
    format!("{branch} moved outside this task")
}

/// The steps the profile lists for before an attempt (ADR 0048 decision 7). A worktree this
/// attempt created already starts from the fetched main, so no step runs for it. Returns the
/// brief the worker receives: a step that leaves the worktree mid-merge puts its section
/// first. The brief stored in the task file never changes.
fn before_attempt(o: &Options, s: &Setup, created: bool, brief: String) -> Result<String, Failure> {
    if created {
        return Ok(brief);
    }
    let mut brief = brief;
    for step in before_attempt_steps(&s.repo)? {
        if step == crate::gate::MERGE_MAIN
            && let Some(conflict) = merge_main(&s.repo, &s.worktree, o, &s.branch, &s.clock)?
        {
            brief = format!("{}\n\n{brief}", conflict_section(&conflict));
        }
    }
    Ok(brief)
}

/// The selected profile's `before_attempt` steps. A repository arc is not set up in has no
/// profile, so no step runs there, the same way the remote and main flags fall back to the git
/// defaults. A `config.toml` that does not parse fails the attempt instead of being ignored:
/// a skipped merge would reach the gate as a conflict (ADR 0048 decisions 7 and 10).
fn before_attempt_steps(repo: &Path) -> Result<Vec<String>, Failure> {
    match crate::store::Store::open(repo) {
        Ok(store) => {
            let config = store
                .config()
                .map_err(|error| Failure::error(error.message()))?;
            Ok(config
                .gate
                .selected_profile()
                .map(|profile| profile.before_attempt.clone())
                .unwrap_or_default())
        }
        Err(crate::store::Error::NotInitialized) => Ok(Vec::new()),
        Err(error) => Err(Failure::error(error.message())),
    }
}

/// One merge of the remote main that conflicted: what the section of the brief reports.
struct MergeConflict {
    /// The ref that was merged, `<remote>/<main>`.
    main_ref: String,
    /// The remote main commit the merge started from.
    main_sha: String,
    /// The conflicted paths, sorted.
    paths: Vec<String>,
}

/// `merge-main`: fetch the remote main with the worktree setup's explicit refspec and merge it
/// into the branch with `--no-ff` (the repository sets `merge.ff=only`, and arc never
/// rebases). `None` when main is already an ancestor or the merge was clean; `Some` when the
/// merge conflicted and the worktree stays mid-merge. A fetch, or a merge that fails for
/// another reason, fails the attempt with git's message, the way a failed worktree setup does.
fn merge_main(
    repo: &Path,
    worktree: &Path,
    o: &Options,
    branch: &str,
    clock: &Clock,
) -> Result<Option<MergeConflict>, Failure> {
    let main_ref = format!("refs/remotes/{}/{}", o.remote, o.main_branch);
    let short_ref = format!("{}/{}", o.remote, o.main_branch);
    let refspec = format!("+refs/heads/{0}:{1}", o.main_branch, main_ref);
    match git(repo, &["fetch", &o.remote, &refspec], clock) {
        Ok(_) => {}
        Err(failure) if failure.wall => return Err(failure),
        Err(failure) => {
            return Err(Failure::error(format!(
                "cannot fetch {} {}: {}",
                o.remote, o.main_branch, failure.message
            )));
        }
    }
    // `merge-base --is-ancestor` answers with its exit code: 0 when main is already in the
    // branch, and anything else means a merge is needed (or the ref could not be read, which
    // the merge below then reports).
    let ancestor = git_raw(
        worktree,
        &["merge-base", "--is-ancestor", &main_ref, branch],
        clock,
    )?;
    if ancestor.status.success() {
        return Ok(None);
    }
    let main_sha = git(repo, &["rev-parse", &main_ref], clock)?
        .trim()
        .to_owned();
    let merge = git_raw(
        worktree,
        &["merge", "--no-ff", "--no-edit", &main_ref],
        clock,
    )?;
    if merge.status.success() {
        return Ok(None);
    }
    let paths = conflicted_paths(worktree, clock)?;
    if paths.is_empty() {
        return Err(Failure::error(format!(
            "cannot merge {short_ref} into {branch}: {}",
            merge.stderr.trim()
        )));
    }
    Ok(Some(MergeConflict {
        main_ref: short_ref,
        main_sha,
        paths,
    }))
}

/// The paths a conflicted merge left unmerged, sorted.
fn conflicted_paths(worktree: &Path, clock: &Clock) -> Result<Vec<String>, Failure> {
    let out = git_raw(
        worktree,
        &["diff", "--name-only", "--diff-filter=U", "-z"],
        clock,
    )?;
    if !out.status.success() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<String> = out
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The section that goes at the top of the brief when `merge-main` left the worktree
/// mid-merge: the ref and its short sha, the conflicted paths, and what to do first.
fn conflict_section(conflict: &MergeConflict) -> String {
    let short = &conflict.main_sha[..conflict.main_sha.len().min(7)];
    let paths = conflict
        .paths
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "## Before you start: the worktree is merging {} ({short})\n\n\
         The worktree is mid-merge: {ref} ({short}) is being merged into your branch, and these \
         paths conflict:\n\n\
         {paths}\n\n\
         Resolve those conflicts first, keeping both sides' intent, and commit the merge with \
         `git commit --no-edit` before you do anything else.",
        conflict.main_ref,
        ref = conflict.main_ref,
    )
}

/// The merge base of `a` and `b`, when git finds one.
fn merge_base(repo: &Path, a: &str, b: &str, clock: &Clock) -> Result<Option<String>, Failure> {
    let out = git_raw(repo, &["merge-base", a, b], clock)?;
    if !out.status.success() {
        return Ok(None);
    }
    let sha = out.stdout.trim();
    Ok((!sha.is_empty()).then(|| sha.to_owned()))
}

/// A start refusal: no child runs and nothing is pushed, but the attempt still ends with its
/// `AttemptFinished` record so the scheduling loop reads the reason. The record is not
/// written as `last_head`: the moved tip is exactly what a later attempt must keep refusing.
fn refuse(o: &Options, s: &Setup, base_sha: &str, reason: &str) -> Result<i32, Failure> {
    write_finish(
        o,
        s,
        Finish {
            base_sha,
            head_sha: None,
            dirty: Vec::new(),
            pushed: false,
            remote_sha: None,
            outcome: "terminated",
            exit_code: EXIT_ERROR,
            failure: Some(reason.to_owned()),
        },
    )
}

/// `refs/heads/<branch>` in the main clone, when it exists.
fn branch_tip(repo: &Path, branch: &str, clock: &Clock) -> Result<Option<String>, Failure> {
    let reference = format!("refs/heads/{branch}");
    let out = git_raw(
        repo,
        &["rev-parse", "--verify", "--quiet", &reference],
        clock,
    )?;
    if !out.status.success() {
        return Ok(None);
    }
    let sha = out.stdout.trim().to_owned();
    Ok((!sha.is_empty()).then_some(sha))
}

/// Takes the exclusive flock on the worktree directory. A second attempt of the same task
/// exits 4 without starting a worker (ADR 0048 decisions 1 and 7.7).
fn lock_worktree(worktree: &Path) -> Result<File, Failure> {
    let file = File::open(worktree).map_err(|error| {
        Failure::error(format!(
            "cannot open {} for the lock: {error}",
            worktree.display()
        ))
    })?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(Failure {
            message: format!("another attempt holds the lock on {}", worktree.display()),
            code: EXIT_LOCKED,
            wall: false,
        }),
        Err(error) => Err(Failure::error(format!(
            "cannot lock {}: {error}",
            worktree.display()
        ))),
    }
}

/// Every entry of `<repository>/.radian/arc/` except `worktrees/`, plus any entry that
/// exists now (ADR 0048 decision 5). The worker's workspace is under `worktrees/`, so no
/// entry here covers it; the sandbox's workspace-inside-a-denied-path check stays.
fn deny_paths(arc: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = DENIED_ENTRIES.iter().map(|name| arc.join(name)).collect();
    if let Ok(entries) = fs::read_dir(arc) {
        for entry in entries.flatten() {
            if entry.file_name() == "worktrees" {
                continue;
            }
            let path = entry.path();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Whether the worker's session directory holds `<task>.jsonl`, the file `radian exec
/// --resume <task>` opens. The directory is the one the worker resolves: a `--session-dir`
/// among the extra arguments, else `RADIAN_SESSION_DIR`, else the user's data directory.
fn session_exists(task: &str, extra_args: &[String]) -> bool {
    let flag = session_dir_arg(extra_args);
    radian_host::session_dir(flag.as_deref())
        .is_ok_and(|dir| dir.join(format!("{task}.jsonl")).is_file())
}

/// The last `--session-dir` value in `args` (`--session-dir <dir>` or `--session-dir=<dir>`).
fn session_dir_arg(args: &[String]) -> Option<PathBuf> {
    let mut found = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--session-dir" {
            found = args.next().map(PathBuf::from);
        } else if let Some(dir) = arg.strip_prefix("--session-dir=") {
            found = Some(PathBuf::from(dir));
        }
    }
    found
}

fn spawn_child(
    o: &Options,
    worktree: &Path,
    deny: &[PathBuf],
    result_path: &Path,
    stderr_path: &Path,
    brief: &str,
) -> Result<Child, Failure> {
    let mut command = match &o.worker_command {
        Some(shell) => {
            // Test-only (see [`Options::worker_command`]). `sh` starts in the wrapper's own
            // working directory and its background children stay in its process group, so
            // the group signal this module forwards can be observed.
            let mut command = Command::new("sh");
            command.arg("-c").arg(shell);
            command
        }
        None => {
            let executable = std::env::current_exe().map_err(|error| {
                Failure::error(format!("cannot find the current executable: {error}"))
            })?;
            let mut command = Command::new(executable);
            command.arg("exec");
            // `--session-id` and `--resume` are exclusive in `radian exec` (ADR 0022/0023): a
            // later attempt resumes the task's session, and the attempt that creates it names
            // it. The attempt number alone does not say which: a first attempt that failed
            // before the model started (a bad worker argument) created nothing to resume.
            if o.resume && session_exists(&o.task, &o.extra_args) {
                command.arg("--resume").arg(&o.task);
            } else {
                command.arg("--session-id").arg(&o.task);
            }
            command
                .arg("--workspace")
                .arg(worktree)
                .arg("--profile")
                .arg("workspace")
                .arg("--yes")
                .arg("--output")
                .arg("jsonl");
            for path in deny {
                command.arg("--deny-read").arg(path);
            }
            command.args(&o.extra_args);
            command.arg("-p").arg("-");
            command
        }
    };
    // The worker gets its own process group, so the SIGTERM the wrapper forwards reaches it
    // (and every child that stays in the group) and never the wrapper or the scheduling
    // process above it (ADR 0048 decision 9).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let stdout = File::create(result_path).map_err(|error| {
        Failure::error(format!("cannot create {}: {error}", result_path.display()))
    })?;
    let stderr = File::create(stderr_path).map_err(|error| {
        Failure::error(format!("cannot create {}: {error}", stderr_path.display()))
    })?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|error| Failure::error(format!("cannot start the worker: {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A child that exits before reading (a usage error) closes the pipe: its own
        // report is in `result.jsonl`/`stderr.log`, and the outcome is `terminated`.
        let _ = stdin.write_all(brief.as_bytes());
    }
    Ok(child)
}

/// What the delivery checks read (ADR 0048 decisions 7.4 and 8).
struct Delivery<'a> {
    repo: &'a Path,
    worktree: &'a Path,
    branch: &'a str,
    base: &'a str,
    /// The `work/<task>` tip this attempt started from: the merge the steps made, if any, is
    /// already in it.
    start: &'a str,
    /// The ref that names the remote main, for the task's own change against main.
    main: &'a str,
    head: Option<&'a str>,
    status_paths: &'a Result<Vec<String>, Failure>,
    allow_noop: bool,
}

/// The checks of ADR 0048 decisions 7.4 and 8, in order. The first failure names its
/// reason; nothing is pushed when one fails. A failure with `wall` set is the clock, not a
/// delivery reason.
fn check_delivery(d: &Delivery<'_>, clock: &Clock) -> Result<(), Failure> {
    let branch = d.branch;
    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_raw(
        d.repo,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
        clock,
    )?;
    if !exists.status.success() {
        return Err(Failure::error(format!("branch {branch} does not exist")));
    }
    let head = d
        .head
        .ok_or_else(|| Failure::error("cannot read the worktree HEAD".to_owned()))?;
    let ancestor = git_raw(
        d.repo,
        &["merge-base", "--is-ancestor", d.base, head],
        clock,
    )?;
    if !ancestor.status.success() {
        return Err(Failure::error(format!(
            "head {head} does not descend from base {}",
            d.base
        )));
    }
    // The child may have rewritten its own branch. It must still contain the tip this
    // attempt started from; otherwise the attempt would deliver a rollback.
    let from_start = git_raw(
        d.repo,
        &["merge-base", "--is-ancestor", d.start, head],
        clock,
    )?;
    if !from_start.status.success() {
        return Err(Failure::error(format!(
            "head {head} does not descend from the start {}",
            d.start
        )));
    }
    let head_ref = git_raw(d.worktree, &["symbolic-ref", "--short", "HEAD"], clock)?;
    if !head_ref.status.success() || head_ref.stdout.trim() != branch {
        return Err(Failure::error(format!(
            "the worktree HEAD is not on {branch}"
        )));
    }
    let dirty = match d.status_paths {
        Ok(paths) => paths,
        Err(failure) if failure.wall => return Err(Failure::wall()),
        Err(failure) => return Err(Failure::error(failure.message.clone())),
    };
    if !dirty.is_empty() {
        return Err(Failure::error(format!(
            "uncommitted or untracked files: {}",
            dirty.join(", ")
        )));
    }
    let changed = changed_paths(d.repo, d.start, head, clock)?;
    if changed.is_empty() && !d.allow_noop {
        return Err(Failure::error(format!(
            "no change: {head} adds nothing to {}",
            d.start
        )));
    }
    // A merge of main brings main's files into the branch. The arc check reads the task's own
    // change against the main the attempt ran on, so a merge commit never trips it (ADR 0048
    // decision 8); a fallback to the recorded base covers a repository whose main ref could
    // not be read.
    let against_main =
        merge_base(d.repo, d.main, head, clock)?.unwrap_or_else(|| d.base.to_owned());
    let changed = changed_paths(d.repo, &against_main, head, clock)?;
    let arc_paths: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| *path == ".radian/arc" || path.starts_with(".radian/arc/"))
        .collect();
    if !arc_paths.is_empty() {
        return Err(Failure::error(format!(
            "the branch changes files under .radian/arc/: {}",
            arc_paths.join(", ")
        )));
    }
    Ok(())
}

fn changed_paths(
    repo: &Path,
    base: &str,
    head: &str,
    clock: &Clock,
) -> Result<Vec<String>, Failure> {
    let range = format!("{base}..{head}");
    let out = git_raw(
        repo,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            &range,
        ],
        clock,
    )?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "cannot list the branch's changes: {}",
            out.stderr.trim()
        )));
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Uncommitted and untracked paths of the worktree, from `git status --porcelain -z`.
fn dirty_paths(worktree: &Path, clock: &Clock) -> Result<Vec<String>, Failure> {
    let out = git_raw(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        clock,
    )?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "cannot list the worktree status: {}",
            out.stderr.trim()
        )));
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|record| record.len() > 3)
        .map(|record| record[3..].to_owned())
        .collect())
}

fn push(repo: &Path, remote: &str, branch: &str, clock: &Clock) -> Result<(), Failure> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = git_raw(repo, &["push", remote, &refspec], clock)?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "push failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(())
}

fn remote_sha_of(
    repo: &Path,
    remote: &str,
    branch: &str,
    clock: &Clock,
) -> Result<String, Failure> {
    let reference = format!("refs/heads/{branch}");
    let out = git_raw(repo, &["ls-remote", remote, &reference], clock)?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "ls-remote failed: {}",
            out.stderr.trim()
        )));
    }
    Ok(out
        .stdout
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// The last `Result` record in the child's jsonl stream, if the child wrote one. A child
/// that crashed or was killed has none, and the attempt's outcome is `terminated`.
fn last_result(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("Result") {
            return Some(value);
        }
    }
    None
}

/// A git command of the wrapper: the crate's one helper, so hooks are empty, the system
/// configuration is out, and an unattended command never waits on a credential prompt. The
/// attempt's clock bounds it: a command still running at the deadline is killed with its
/// process group, `signal.json` gets `wall_time`, and the caller sees a `wall` failure.
fn git_raw(cwd: &Path, args: &[&str], clock: &Clock) -> Result<GitOutput, Failure> {
    if clock.expired() {
        clock.mark()?;
        return Err(Failure::wall());
    }
    match crate::git::run_with_timeout(cwd, args, clock.remaining()) {
        Ok(crate::git::TimedRun::Done(output)) => Ok(output),
        Ok(crate::git::TimedRun::TimedOut) => {
            clock.mark()?;
            Err(Failure::wall())
        }
        Err(error) => Err(Failure::error(error.to_string())),
    }
}

fn git(cwd: &Path, args: &[&str], clock: &Clock) -> Result<String, Failure> {
    let out = git_raw(cwd, args, clock)?;
    if !out.status.success() {
        return Err(Failure::error(format!(
            "git {} failed: {}",
            args.join(" "),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a) == canonical(b)
}

fn exit_code_of(status: &ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    -1
}

/// The worker's process start time, for the host-local identity record (ADR 0048 decision 4).
/// `ps`'s own spelling is recorded verbatim; a failure falls back to the wrapper's spawn time.
fn process_start(pid: u32) -> Option<String> {
    process::start_time(pid)
}

fn hostname() -> Option<String> {
    let output = Command::new("hostname").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn read_stdin() -> Result<String, Failure> {
    let mut brief = String::new();
    std::io::stdin()
        .read_to_string(&mut brief)
        .map_err(|error| Failure::error(format!("cannot read the brief from stdin: {error}")))?;
    Ok(brief)
}

fn write_json(path: &Path, value: &Value) -> Result<(), Failure> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| Failure::error(format!("cannot serialize {}: {error}", path.display())))?;
    write_text(path, &text)
}

fn write_text(path: &Path, text: &str) -> Result<(), Failure> {
    fs::write(path, format!("{text}\n"))
        .map_err(|error| Failure::error(format!("cannot write {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decision 5: the worker's deny list is every entry of the arc directory except
    /// `worktrees/`, including the two the store creates after the wrapper was written
    /// (`gates/` and `ledger/`). The worker must not read the gate checkouts or the ledger.
    #[test]
    fn the_deny_list_covers_the_whole_arc_layout_except_the_worktree() {
        for name in [
            "config.toml",
            "plans",
            "tasks",
            "ledger",
            "runs",
            "attempts",
            "gates",
            "state.lock",
            "runner.lock",
        ] {
            assert!(DENIED_ENTRIES.contains(&name), "{name} is not denied");
        }
        assert!(
            !DENIED_ENTRIES.contains(&"worktrees"),
            "the worker's own workspace is not denied"
        );
        assert_eq!(DENIED_ENTRIES.len(), 9, "one entry per layout item");
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    /// Decision 7.2: a later attempt resumes only a session that exists. A first attempt
    /// that failed on a bad worker argument created no session file, and resuming it would
    /// fail every remaining attempt with `no session`.
    #[test]
    fn a_session_exists_only_when_its_file_is_in_the_worker_session_dir() {
        let dir = std::env::temp_dir().join(format!("radian-arc-session-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("session dir");
        let path = dir.to_str().expect("utf-8 path");
        let args = strings(&["--model", "m", "--session-dir", path]);
        assert!(!session_exists("TK0001-task", &args), "no file yet");
        fs::write(dir.join("TK0001-task.jsonl"), "").expect("write session");
        assert!(session_exists("TK0001-task", &args));
        assert!(
            !session_exists("TK0002-other", &args),
            "another task's file"
        );
        let equals = strings(&[&format!("--session-dir={path}")]);
        assert!(session_exists("TK0001-task", &equals));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_last_session_dir_argument_wins() {
        assert_eq!(session_dir_arg(&strings(&["--model", "m"])), None);
        assert_eq!(
            session_dir_arg(&strings(&["--session-dir", "/a", "--session-dir=/b"])),
            Some(PathBuf::from("/b"))
        );
        assert_eq!(
            session_dir_arg(&strings(&["--session-dir=/a", "--session-dir", "/b"])),
            Some(PathBuf::from("/b"))
        );
    }
}
