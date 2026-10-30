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
//! 3. run the attempt's worker. A `radian` worker is `radian acp` spoken to over the ACP
//!    client ([`crate::acp`]) with the flags the `radian exec` worker used to get
//!    (`--session-id <session>` or `--resume <session>`, `--workspace <worktree>`, `--profile
//!    workspace`, the deny list and the caller's extra arguments) and the brief as its one
//!    `session/prompt`; the flow's `acp` program is started the same way, and a flow's
//!    `command` (or the test-only shell) writes JSON lines to stdout into
//!    `runs/…/result.jsonl`;
//! 4. while the child runs, enforce the attempt's limits here: a stall, the wall time, or a
//!    `cancel` records `signal.json`, sends SIGTERM to the child's process group and then
//!    SIGKILL after the grace period. These limits belong to this process, so they hold after
//!    the `run` process that started it exits or is killed (decision 7.8).
//!    A heartbeat thread writes `attempts/<task>/<attempt>/heartbeat.json` for a later pass
//!    to judge liveness (decision 7.9);
//! 5. after the child exits, compare the repository's git control files with the record taken
//!    before the child started, restore them on a change, and fail the attempt (decision 7);
//!    when the flow sets `commit_dirty` and the outcome is `completed`, commit a worktree the
//!    worker left dirty (decision 8); then read the `Result` line and the exit code, read
//!    `head_sha`, list uncommitted and untracked files, and run the delivery checks;
//! 6. push `work/<task>` only when the outcome is `completed`, the git control files are
//!    unchanged, and every check passed, then verify the remote ref with `ls-remote`;
//! 7. print one `AttemptFinished` JSON line as its last stdout line and exit with the
//!    child's exit code (or 1 when a check failed after a `completed` child).
//!
//! Every git command this module runs goes through the crate's one git helper
//! (`crate::git`): built from the shared host builder, an empty hooks path, and
//! `GIT_CONFIG_NOSYSTEM=1`, so no file the model can write changes what the wrapper runs
//! (ADR 0048 decision 7.3). That does not cover a worker that writes a filter driver or
//! `core.fsmonitor` into `.git/config`, so the wrapper also records the repository's git
//! control files before the worker and compares them after it exits (step 5). The push
//! credential stays in this process's environment; in v0 the wrapper and the model are the
//! same OS user, so that is not isolation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde_json::{Value, json};

use crate::acp;
use crate::gate::{
    BeforeAttempt, BeforeAttemptCommand, DEFAULT_COMMAND_TIMEOUT_SECS, ShellWatch, run_shell,
};
use crate::git::GitOutput;
use crate::process;
use crate::store::attempt_ops::{HEARTBEAT_FILE, SIGNAL_FILE, heartbeat_interval};

/// How often the wrapper looks at the worker, its jsonl stream and a cancel signal.
const POLL: Duration = Duration::from_millis(100);

/// How long the wrapper waits after SIGKILL for the worker's process group to disappear: a
/// killed process is reaped by its new parent, not by the wrapper (ADR 0048 decisions 7 and 8).
const GROUP_REAP: Duration = Duration::from_secs(5);

/// The `radian arc work` flags. `radian-tui`'s `arc work` subcommand fills this.
pub struct Options {
    /// The task id: the branch suffix and the worktree name.
    pub task: String,
    /// The attempt number, 1-based.
    pub attempt: u32,
    /// The random token the scheduling loop expects back in `AttemptFinished`.
    pub token: String,
    /// The worker's session id (ADR 0048 decision 4): the task id with `[worker]
    /// rework_session = "resume"`, `<task>-<attempt>` with `fresh`. arc sets it from the
    /// claim; absent on the command line, it is the task id.
    pub session: String,
    /// A `resume`-mode later attempt: continue the session (`--resume <session>`) when it
    /// exists. A flag without a session file starts one, as before: an attempt that failed
    /// before the model started leaves nothing to resume. Without this flag an attempt after
    /// the first is a fresh session and gets the hand-off section, which is why the flag is
    /// also the mode's signal here.
    pub resume: bool,
    /// Let a branch with no change past `base_sha` pass the delivery check (ADR 0048
    /// decision 8). The claim resolves this to the task's effective `allow_noop`: its own
    /// value when its file carries one, else its flow's `flows.<name>.allow_noop`.
    pub allow_noop: bool,
    /// Commit what a `completed` worker left uncommitted or untracked before the delivery
    /// check (ADR 0048 decision 8). The claim passes its flow's `flows.<name>.commit_dirty`;
    /// `false` keeps the rule that a dirty worktree fails the attempt.
    pub commit_dirty: bool,
    /// The remote the wrapper fetches the main branch from and pushes `work/<task>` to.
    /// Internal: `arc` sets it from the repository configuration (default `origin`); a
    /// person does not.
    pub remote: String,
    /// The branch a first attempt starts from and `merge-main` merges: `[integration]
    /// target_branch`, else `[repository] main_branch`. Internal: `arc` sets it; a person does
    /// not.
    pub main_branch: String,
    /// Extra worker arguments after `--`: the flow's `exec_args` (the model route in tests:
    /// `--model`, `--base-url`). They reach `radian acp` for a `radian` worker and the
    /// configured program for a `command` or `acp` worker.
    pub extra_args: Vec<String>,
    /// Internal: the configured `[worker] command`, the program first and its arguments after
    /// it (ADR 0048 decision 7). arc reads it at the claim and passes it here; unset runs
    /// `radian acp`. Never set by a person; it is not in the manual.
    pub command: Option<Vec<String>>,
    /// Internal: a flow's `acp`, the program first and its arguments after it: the wrapper is
    /// the ACP client and starts this array as the agent (ADR 0048 decision 7). arc reads it at
    /// the claim and passes it here. Never set by a person; it is not in the manual.
    pub acp: Option<Vec<String>>,
    /// Internal: the task's flow, the name the attempt's records carry (ADR 0048 decision 15).
    /// arc passes the flow the claim resolved; a person does not set it.
    pub flow: String,
    /// Internal: the attempt's stall limit in seconds; `0` means no limit. arc passes the
    /// task's `stall_timeout_secs`, else `[limits] stall_timeout_secs`. The wrapper enforces
    /// it, so the limit survives the `run` process that started it (ADR 0048 decision 7.8).
    pub stall_timeout_secs: u64,
    /// Internal: the attempt's absolute wall-time limit in seconds; `0` means no limit.
    pub max_wall_time_secs: u64,
    /// Internal: how long a signalled worker gets before SIGKILL (`[limits]
    /// cancel_grace_secs`).
    pub cancel_grace_secs: u64,
    /// Test-only: run this shell command as the worker instead of the attempt's own worker. It
    /// exists so the forwarding tests can start a plain process that spawns a child of its own,
    /// which neither `radian acp` nor a command worker does in the same way. Never set by
    /// arc; an input that exists only for tests is not documented (AGENTS.md).
    pub worker_command: Option<String>,
}

/// Exit codes (ADR 0048 decision 1): 0 success, 1 error, 2 usage, 4 a lock is held.
const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_LOCKED: i32 = 4;

/// The environment variables the wrapper adds for a command worker (ADR 0048 decision 7). One
/// list for the wrapper and the manual's drift test. They are set with [`Command::env`] by
/// name, so the source scan in `radian-host/tests/env_docs.rs` does not take them for inputs a
/// person sets: they describe what the command receives, and the arc manual documents them.
pub const WORKER_ENV: &[&str] = &[
    "ARC_WORKTREE",
    "ARC_TASK",
    "ARC_ATTEMPT",
    "ARC_SESSION",
    "ARC_SESSION_START",
    "ARC_GIT_DIR",
    "ARC_GIT_COMMON_DIR",
    "ARC_DENY_READ",
    "ARC_STATE_DIR",
];

/// The worker a run starts: the test-only shell, the configured ACP agent, the configured
/// command, or `radian acp` (ADR 0048 decision 7). The test-only shell takes precedence, so
/// every existing forwarding test keeps its shape.
#[derive(Debug, Clone, Copy)]
enum Worker<'a> {
    /// `radian acp`: the same agent `radian exec` served headless, spoken to as ACP.
    Radian,
    Command(&'a [String]),
    Acp(&'a [String]),
    Shell(&'a str),
}

fn worker_of(o: &Options) -> Worker<'_> {
    if let Some(shell) = o.worker_command.as_deref() {
        Worker::Shell(shell)
    } else if let Some(acp) = o.acp.as_deref() {
        Worker::Acp(acp)
    } else if let Some(command) = o.command.as_deref() {
        Worker::Command(command)
    } else {
        Worker::Radian
    }
}

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
    let wall = Duration::from_secs(o.max_wall_time_secs);
    let repo = repo_root(started, wall)?;
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
        clock: Clock::new(started, wall, signal),
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
            notes: &[],
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

    /// Whether the wrapper was asked to stop: a SIGTERM/SIGINT the handler saw, or the record
    /// `radian arc cancel` leaves in the attempt's run directory (ADR 0048 decision 9).
    fn stopped(&self) -> bool {
        STOP_REQUESTED.load(Ordering::SeqCst) || self.signal.is_file()
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
    let brief = read_stdin(&s.clock)?;
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
    // The profile's entries before an attempt: `merge-main` and the custom commands, in the
    // order listed (ADR 0048 decisions 7 and 10). The repository's git control files are
    // recorded first, so an entry that changes one fails the attempt the same way a worker
    // does: the check runs again after the entries and before the worker starts. It runs even
    // when an entry failed, so a command that changed a control file and then failed is put
    // back before this attempt ends.
    let control = GitControl::record(&s.worktree, &s.clock)?;
    let before = before_attempt(o, s, created);
    let check = control.check();
    // A restore that failed wins over the entry's own failure, exactly as it does after a
    // worker: the attempt ends here with the control-check reason, so no later step runs on a
    // control file the wrapper could not put back. The entry's failure stays as context.
    if let ControlCheck::Changed(change) = &check
        && change.restore_failed.is_some()
    {
        let failure = match &before {
            Ok(_) => change.reason(),
            Err(before) => format!("{}; {}", change.reason(), before.message),
        };
        return write_finish(
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
                failure: Some(failure),
                notes: &[],
            },
        );
    }
    let conflict = before?;
    if let ControlCheck::Changed(change) = check {
        return write_finish(
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
                failure: Some(change.reason()),
                notes: &[],
            },
        );
    }
    let start_sha = branch_tip(&s.repo, &s.branch, &s.clock)?.ok_or_else(|| {
        Failure::error(format!("cannot read {} in {}", s.branch, s.repo.display()))
    })?;
    // A fresh rework gets the previous attempt's end in front of the brief: its new session
    // cannot see that conversation (ADR 0048 decision 7).
    let handoff = handoff_section(o, s, &start_sha, &base_sha)?;
    // The repository's standing worker instructions end every brief (ADR 0048 decision 7).
    // They are read here, when the brief is composed, so an edit takes effect at the next
    // attempt; a file that is too large, not UTF-8, or unreadable is skipped with a note.
    let (footer, skipped) = worker_brief(&s.arc);
    let mut notes: Vec<String> = skipped.into_iter().collect();
    let brief = compose_brief(&[
        conflict.as_deref(),
        handoff.as_deref(),
        Some(&brief),
        footer.as_deref(),
    ]);
    // A path the brief names under `runs/` reaches the worker as a copy in its worktree (ADR
    // 0048 decision 7): the sandbox still denies `.radian/arc/`, and the task file keeps the
    // absolute path.
    let (brief, copy_notes) = copy_verdicts(&brief, &s.arc, &s.worktree);
    notes.extend(copy_notes);
    let started_at = now();
    let deny = deny_paths(&s.arc);
    // The command worker's own state, outside the worktree and kept across the task's
    // attempts (ADR 0048 decision 7). It exists before the command starts.
    let state_dir = s.arc.join("attempts").join(&o.task).join("state");
    if matches!(worker_of(o), Worker::Command(_) | Worker::Acp(_)) {
        fs::create_dir_all(&state_dir).map_err(|error| {
            Failure::error(format!("cannot create {}: {error}", state_dir.display()))
        })?;
    }
    // What this attempt runs, for the attempt's own record (ADR 0048 decision 7).
    let worker = match worker_of(o) {
        Worker::Radian => json!({"kind": "radian", "flow": o.flow}),
        Worker::Command(words) => json!({
            "kind": "command",
            "flow": o.flow,
            "program": words.first().cloned().unwrap_or_default(),
        }),
        Worker::Acp(words) => json!({
            "kind": "acp",
            "flow": o.flow,
            "program": words.first().cloned().unwrap_or_default(),
        }),
        Worker::Shell(_) => json!({"kind": "command", "flow": o.flow, "program": "sh"}),
    };
    // What this attempt does with its session, decided once (ADR 0048 decision 4): the
    // `--session-id`/`--resume` flag and the `session/new`/`session/load` request read the
    // same value, so a session file that appears between the two cannot split them.
    let start = session_start(o.resume, session_exists(&o.session, &o.extra_args));
    let spawn = Spawn {
        worktree: &s.worktree,
        deny: &deny,
        state_dir: &state_dir,
        control: &control,
        result_path: &result_path,
        stderr_path: &stderr_path,
        brief: &brief,
        start,
    };
    let (mut child, acp_log) = match worker_of(o) {
        Worker::Radian => {
            let (child, log) = spawn_radian_acp(o, &spawn)?;
            (child, Some(log))
        }
        Worker::Acp(words) => {
            let (child, log) = spawn_acp(o, &spawn, words)?;
            (child, Some(log))
        }
        _ => (spawn_child(o, &spawn)?, None),
    };
    let pid = child.id();
    // An ACP agent is stopped through the protocol first, so the SIGTERM handler must not
    // forward the wrapper's own signal to its group before the wrapper sent `session/cancel`
    // (ADR 0048 decision 9). The wrapper's monitor reads the signal instead.
    if acp_log.is_none() {
        CHILD_PID.store(pid as i32, Ordering::SeqCst);
        // A SIGTERM that arrived during the worktree setup is forwarded as soon as there is a
        // child to forward it to.
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            process::signal_group(pid, process::SIGTERM);
        }
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
            "session_id": o.session,
            // The worker this attempt started, so a reader can classify a running attempt
            // without reading `config.toml`, which may have changed since the claim
            // (ADR 0048 decision 4).
            "worker": worker.clone(),
            "worktree": s.worktree.display().to_string(),
            "branch": s.branch,
            "started_at": started_at,
        }),
    )?;
    // The limits belong to this process, not to the scheduling loop that started it: a
    // one-shot `run`, or a killed one, leaves the worker bounded (decision 7.8).
    let (status, acp_failure) = match acp_log {
        Some(log) => {
            let mut client = acp::Client::start(&mut child, s.worktree.clone(), log.clone())
                .map_err(Failure::error)?;
            let turn = run_acp_turn(o, s, &mut child, &mut client, &brief, &log, start)?;
            (turn.exit_code, turn.failure)
        }
        None => {
            let status = monitor_child(
                &mut child,
                &result_path,
                &s.runs.join(SIGNAL_FILE),
                &Limits::of(o),
                s.clock.deadline(),
            )?;
            (exit_code_of(&status), None)
        }
    };
    CHILD_PID.store(0, Ordering::SeqCst);
    let ended_at = now();
    let exit_code = status;

    let result = last_result(&result_path);
    let outcome = result
        .as_ref()
        .and_then(|value| value.get("outcome"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| "terminated".to_owned());

    // ADR 0048 decisions 7 and 8: a worker that changed the repository's git control files
    // fails the attempt, and they are put back before this wrapper runs its next git command.
    // A control file the check cannot read back counts as a change and is restored too; a
    // restore that fails stops the attempt here, so no git command runs on files the wrapper
    // could not repair.
    let mut failure = acp_failure;
    match control.check() {
        ControlCheck::Unchanged => {}
        ControlCheck::Changed(change) if change.restore_failed.is_some() => {
            let reason = match &failure {
                Some(before) => format!("{}; {before}", change.reason()),
                None => change.reason(),
            };
            return write_finish(
                o,
                s,
                Finish {
                    base_sha: &base_sha,
                    head_sha: None,
                    dirty: Vec::new(),
                    pushed: false,
                    remote_sha: None,
                    outcome: &outcome,
                    exit_code,
                    failure: Some(reason),
                    notes: &notes,
                },
            );
        }
        ControlCheck::Changed(change) => failure = Some(change.reason()),
    }

    // `flows.<name>.commit_dirty` (ADR 0048 decision 8): after the control-file check and
    // before the delivery check, a `completed` worker's dirty worktree is committed on the
    // branch. A changed control file already gave the attempt its reason, and a failed or
    // cancelled worker's changes stay uncommitted.
    if failure.is_none() && outcome == "completed" && o.commit_dirty {
        commit_dirty(o, s, result.as_ref(), &mut notes)?;
    }

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
            "session_id": o.session,
            "worktree": s.worktree.display().to_string(),
            "branch": s.branch,
            "base_sha": base_sha,
            "head_sha": head_sha,
            "started_at": started_at,
            "ended_at": ended_at,
            // Which worker ran this attempt, so `show` and the arc TUI can tell a command
            // worker from `radian acp` (ADR 0048 decision 7).
            "worker": worker,
            // A line such as `worker-brief.md skipped: not UTF-8`; `show` prints it
            // (ADR 0048 decision 7). Nothing to report leaves an empty list.
            "notes": notes,
        }),
    )?;

    // The wrapper, not the model, decides whether the attempt delivered anything (ADR 0048
    // decisions 7 and 8). Only a `completed` child with every check passed is pushed; a
    // changed git control file already gave the attempt its reason, and nothing is pushed.
    let mut pushed = false;
    let mut remote_sha: Option<String> = None;
    if failure.is_none() {
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
                                        failure = Some(format!(
                                            "remote ref is {remote}, expected {head}"
                                        ));
                                    }
                                    remote_sha = Some(remote);
                                }
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
            notes: &notes,
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
    /// The notes of the attempt, such as the worker brief being skipped (ADR 0048 decision 7).
    notes: &'a [String],
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
        "notes": finish.notes,
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
            notes: &[],
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
                    // Sleep only what is left of the interval: a fixed poll slice would
                    // stretch a 333 ms interval to 400 ms, and three beats would no longer
                    // fit in a 1 s stall window.
                    let now = Instant::now();
                    std::thread::sleep(deadline.saturating_duration_since(now).min(POLL));
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
        // `exited_status` peeks without reaping. The zombie keeps the process-group id
        // reserved, so neither `stop_group` nor the stop handler can signal a group that a
        // reused id names (ADR 0048 decisions 7 and 8).
        if let Some(status) = process::exited_status(child.id())
            .map_err(|error| Failure::error(format!("cannot wait for the worker: {error}")))?
        {
            // The direct child is gone, but a background process it left in its own process
            // group is not necessarily: it could change a git control file after this exit
            // and before the wrapper compares them, or after the attempt ends. Nothing the
            // worker started runs from this point (ADR 0048 decisions 7 and 8). The stop
            // handler is told there is no child first. The zombie is deliberately left for
            // this process's own exit: while it remains, the group id cannot name another
            // group.
            CHILD_PID.store(0, Ordering::SeqCst);
            stop_group(child.id(), limits.grace);
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

/// Stops everything the worker left in its process group after the direct child exits: SIGTERM,
/// then SIGKILL once `grace` runs out, the same way a cancel stops it. The caller has observed
/// the direct child's exit without reaping it, so the zombie still holds the group id: no
/// signal here can reach a reused group. The probe ends the wait as soon as the group has no
/// live member, which is the normal case (ADR 0048 decisions 7 and 8).
fn stop_group(pgid: u32, grace: Duration) {
    if !process::group_has_live_member(pgid) {
        return;
    }
    process::signal_group(pgid, process::SIGTERM);
    let deadline = Instant::now() + grace;
    loop {
        if !process::group_has_live_member(pgid) {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    process::signal_group(pgid, process::SIGKILL);
    // A killed process can stay a zombie for a moment until its parent reaps it. Wait a
    // bounded time, so the check after this does not race the group's last entry.
    let deadline = Instant::now() + GROUP_REAP;
    while process::group_has_live_member(pgid) {
        if Instant::now() >= deadline {
            return;
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
    // The session id becomes a file name in the worker's session directory, so it follows
    // the same rule `radian exec` applies to `--session-id` and `--resume`.
    radian_host::validate_session_id(&o.session).map_err(Failure::usage)?;
    // The configured command is one program and its arguments; an empty list has no program.
    if o.command.as_ref().is_some_and(Vec::is_empty) {
        return Err(Failure::usage(
            "the configured worker command is empty; its first element must be the program",
        ));
    }
    // The configured ACP program is one program and its arguments, as a command is.
    if o.acp.as_ref().is_some_and(Vec::is_empty) {
        return Err(Failure::usage(
            "the configured ACP agent is empty; its first element must be the program",
        ));
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
/// create another one. The attempt's wall clock already covers this local `rev-parse`, so a
/// command that hangs is killed at the deadline like every other wrapper git command; the
/// attempt's own records cannot be written before the root is known.
fn repo_root(started: Instant, wall: Duration) -> Result<PathBuf, Failure> {
    let cwd = std::env::current_dir()
        .map_err(|error| Failure::error(format!("cannot read the current directory: {error}")))?;
    let remaining = (!wall.is_zero()).then(|| wall.saturating_sub(started.elapsed()));
    if remaining.is_some_and(|remaining| remaining.is_zero()) {
        return Err(Failure::wall());
    }
    let out = match crate::git::run_with_timeout(&cwd, &["rev-parse", "--show-toplevel"], remaining)
    {
        Ok(crate::git::TimedRun::Done(out)) => out,
        Ok(crate::git::TimedRun::TimedOut) => return Err(Failure::wall()),
        Err(_) => {
            return Err(Failure::error(format!(
                "{} is not inside a git repository",
                cwd.display()
            )));
        }
    };
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

/// Creates the worktree from the configured remote's target branch when it is missing, or
/// reuses it. Returns whether this run created it (the first attempt records `base_sha` from
/// the fetched target branch and starts there).
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
    // time; the guard lives until this function returns, before the worker starts. A wait
    // that outlives the attempt's wall clock is a wall-time failure, like a git command.
    let _setup = File::open(parent).map_err(|error| {
        Failure::error(format!(
            "cannot open {} for the setup lock: {error}",
            parent.display()
        ))
    })?;
    loop {
        match _setup.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if clock.expired() {
                    clock.mark()?;
                    return Err(Failure::wall());
                }
                let wait = clock.remaining().map_or(POLL, |left| left.min(POLL));
                std::thread::sleep(wait);
            }
            Err(error) => {
                return Err(Failure::error(format!(
                    "cannot lock {} for the setup: {error}",
                    parent.display()
                )));
            }
        }
    }
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
        match fetch_remote(repo, &["fetch", remote, &refspec], clock) {
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
        // `--no-track`: the common config is a git control file the wrapper records before each
        // worker (ADR 0048 decision 7). Starting the branch from `origin/main` would otherwise
        // write `[branch "<branch>"]` there, and a concurrent attempt's record would see it as
        // the worker's change. arc never relies on the branch's upstream: every fetch and push
        // names its remote and refspec.
        git(
            repo,
            &["worktree", "add", "--no-track", "-b", branch, &path, &start],
            clock,
        )?;
    }
    Ok(true)
}

/// The task's base commit: the fetched remote target branch when this run created the
/// worktree, else the value the first attempt recorded. A worktree made outside the wrapper
/// falls back to the merge-base with the configured remote's target branch.
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

/// The entries the profile lists for before an attempt (ADR 0048 decisions 7 and 10). They run
/// in the order listed: `merge-main` does nothing on a worktree this attempt created, which
/// already starts from the fetched main, and a command runs before every attempt, the first
/// included. Returns the section a `merge-main` leaves in front of the brief: a conflict leaves
/// the worktree mid-merge, and the worker resolves it first. The brief stored in the task file
/// never changes.
fn before_attempt(o: &Options, s: &Setup, created: bool) -> Result<Option<String>, Failure> {
    let (entries, default_timeout) = before_attempt_entries(&s.repo, &o.task, &o.flow)?;
    let mut conflict = None;
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            // `Profile::validate` admits only `merge-main`; the wrapper never guesses at a
            // step this build does not run.
            BeforeAttempt::Step(step) => {
                if step == crate::gate::MERGE_MAIN
                    && !created
                    && let Some(found) = merge_main(&s.repo, &s.worktree, o, &s.branch, &s.clock)?
                {
                    conflict = Some(conflict_section(&found));
                }
            }
            BeforeAttempt::Run(command) => {
                run_before_command(o, s, command, index + 1, default_timeout)?;
            }
        }
    }
    Ok(conflict)
}

/// Runs one custom `before_attempt` command (ADR 0048 decisions 7 and 10): `sh -c <run>` in the
/// attempt's worktree with no sandbox, in its own process group, the command worker's
/// `ARC_WORKTREE`, `ARC_TASK` and `ARC_ATTEMPT` added to the wrapper's environment, stdout and
/// stderr into `before/<index>.log`. A non-zero exit, or the command's timeout, ends the attempt
/// before the worker starts. The shorter of the command's timeout and the attempt's remaining
/// wall time applies; the wall clock's own expiry is the attempt's `wall_time` failure, as it is
/// for every other wrapper step.
fn run_before_command(
    o: &Options,
    s: &Setup,
    command: &BeforeAttemptCommand,
    index: usize,
    default_timeout: Duration,
) -> Result<(), Failure> {
    if s.clock.stopped() {
        return Err(stopped_before(index, command));
    }
    if s.clock.expired() {
        s.clock.mark()?;
        return Err(Failure::wall());
    }
    let before = s.runs.join("before");
    fs::create_dir_all(&before)
        .map_err(|error| Failure::error(format!("cannot create {}: {error}", before.display())))?;
    let log = before.join(format!("{index}.log"));
    let configured = command
        .timeout_secs
        .map_or(default_timeout, Duration::from_secs);
    // The wall clock bounds every step of the attempt (ADR 0048 decision 7.8), so a command
    // gets no more than the time that is left. `wall_wins` keeps the failure's class right.
    let (timeout, wall_wins) = match s.clock.remaining() {
        Some(remaining) if configured.is_zero() => (remaining, true),
        Some(remaining) => (remaining.min(configured), remaining <= configured),
        None => (configured, false),
    };
    let absolute = fs::canonicalize(&s.worktree).unwrap_or_else(|_| s.worktree.clone());
    let env = [
        ("ARC_WORKTREE", absolute.display().to_string()),
        ("ARC_TASK", o.task.clone()),
        ("ARC_ATTEMPT", format!("{}-{}", o.task, o.attempt)),
    ];
    // The wrapper's own stop path reaches this command's group: its SIGTERM/SIGINT handler
    // forwards through `CHILD_PID`, and a cancel may only have left `signal.json`. Either ends
    // the attempt, so a command with no timeout cannot hold the task or start a worker after
    // the stop.
    let stop = CommandStop {
        clock: &s.clock,
        grace: Duration::from_secs(o.cancel_grace_secs),
    };
    let result = run_shell(
        &s.worktree,
        &command.run,
        timeout,
        &log,
        &env,
        &[],
        Some(&stop),
    )
    .map_err(|error| Failure::error(format!("before_attempt entry {index}: {error}")))?;
    // A stop wins over the command's own result, as a cancel does for a running worker.
    if s.clock.stopped() {
        return Err(stopped_before(index, command));
    }
    if result.timed_out() {
        if wall_wins {
            s.clock.mark()?;
            return Err(Failure::wall());
        }
        return Err(Failure::error(format!(
            "before_attempt entry {index} timed out after {} s: {}; log: {}",
            timeout.as_secs(),
            command.run,
            log.display()
        )));
    }
    if !result.passed() {
        let status = result.exit_code.map_or_else(
            || "was killed by a signal".to_owned(),
            |code| format!("failed with exit code {code}"),
        );
        return Err(Failure::error(format!(
            "before_attempt entry {index} {status}: {}; log: {}",
            command.run,
            log.display()
        )));
    }
    Ok(())
}

/// The wrapper was asked to stop while a `before_attempt` command was about to run or ran
/// (ADR 0048 decision 9): the attempt ends before the worker, and the command's group has been
/// signalled. A cancel has already recorded its reason; this failure is the attempt's record.
fn stopped_before(index: usize, command: &BeforeAttemptCommand) -> Failure {
    Failure::error(format!(
        "before_attempt entry {index} stopped: {}",
        command.run
    ))
}

/// The wrapper's stop path for one `before_attempt` command (ADR 0048 decisions 7 and 9): the
/// command's process group is the one the SIGTERM handler forwards to, and a cancel's
/// `signal.json` is read the same way the worker monitor reads it.
struct CommandStop<'a> {
    clock: &'a Clock,
    grace: Duration,
}

impl ShellWatch for CommandStop<'_> {
    fn started(&self, pgid: u32) {
        CHILD_PID.store(pgid as i32, Ordering::SeqCst);
    }

    fn finished(&self) {
        CHILD_PID.store(0, Ordering::SeqCst);
    }

    fn stopping(&self) -> bool {
        self.clock.stopped()
    }

    fn grace(&self) -> Duration {
        // The configured grace belongs to the command's group, on a cancel as on a plain
        // SIGTERM. `cancel` leaves the wrapper its own window to reap the group and write its
        // record (`attempt::CANCEL_RECORD_RESERVE`), so nothing here shortens the grace: a
        // grace of one second or less would otherwise become none at all.
        self.grace
    }
}

/// The task's profile's `before_attempt` entries and that profile's default command timeout
/// (ADR 0048 decisions 7, 10 and 15): the flow's `gate_profile`, else `gate.profile`. A
/// repository arc is not set up in has no profile, so no entry runs there, the same way the
/// remote and main flags fall back to the git defaults. A `config.toml` that does not parse
/// fails the attempt instead of being ignored, and so does a flow or profile the file no longer
/// defines: a skipped merge would reach the gate as a conflict, or run the wrong profile's
/// entries.
fn before_attempt_entries(
    repo: &Path,
    task: &str,
    flow: &str,
) -> Result<(Vec<BeforeAttempt>, Duration), Failure> {
    match crate::store::Store::open(repo) {
        Ok(store) => {
            let config = store
                .config()
                .map_err(|error| Failure::error(error.message()))?;
            let (_, profile) = config
                .task_gate_profile(task, flow)
                .map_err(Failure::error)?;
            Ok((
                profile.before_attempt.clone(),
                config.gate.command_timeout(),
            ))
        }
        Err(crate::store::Error::NotInitialized) => Ok((
            Vec::new(),
            Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS),
        )),
        Err(error) => Err(Failure::error(error.message())),
    }
}

/// One merge of the remote target branch that conflicted: what the section of the brief
/// reports.
struct MergeConflict {
    /// The ref that was merged, `<remote>/<target>`.
    main_ref: String,
    /// The remote target commit the merge started from.
    main_sha: String,
    /// The conflicted paths, sorted.
    paths: Vec<String>,
}

/// `merge-main`: fetch the remote target branch with the worktree setup's explicit refspec and
/// merge it into the branch with `--no-ff` (the repository sets `merge.ff=only`, and arc never
/// rebases). `None` when the target is already an ancestor or the merge was clean; `Some` when
/// the merge conflicted and the worktree stays mid-merge. A fetch, or a merge that fails for
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
    match fetch_remote(repo, &["fetch", &o.remote, &refspec], clock) {
        Ok(_) => {}
        Err(failure) if failure.wall => return Err(failure),
        Err(failure) => {
            return Err(Failure::error(format!(
                "cannot fetch {} {}: {}",
                o.remote, o.main_branch, failure.message
            )));
        }
    }
    // `merge-base --is-ancestor` answers with its exit code: 0 when the target branch is
    // already in the branch, and anything else means a merge is needed (or the ref could not
    // be read, which the merge below then reports).
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

/// The report's cap: how many characters of the previous attempt's final report a hand-off
/// carries.
const HANDOFF_REPORT_CHARS: usize = 8_000;

/// The largest `.radian/arc/worker-brief.md` the wrapper appends: 32 KiB. A larger file is
/// skipped with a note; the attempt goes on (ADR 0048 decision 7).
const WORKER_BRIEF_LIMIT: u64 = 32 * 1024;

/// The repository's standing worker instructions (ADR 0048 decision 7): the section every
/// brief ends with, and the note the attempt record carries when the file is not appended.
/// The file is read when the attempt's brief is composed, so an edit takes effect at the next
/// attempt. A missing file and one that trims to nothing are no section and no note. A path
/// that is not a regular file, a file over [`WORKER_BRIEF_LIMIT`], one that is not UTF-8, and a
/// read error are skipped with a note; the attempt still runs. The worker never reads the file
/// itself: its sandbox denies `.radian/arc/`, and the content reaches it on stdin.
fn worker_brief(arc: &Path) -> (Option<String>, Option<String>) {
    let path = arc.join(crate::store::WORKER_BRIEF_FILE);
    // One handle for the checks and the read: `metadata()` on the handle describes the file
    // the bytes come from, so a path swapped after the open cannot change what is read. The
    // open is non-blocking on Unix, so a FIFO or a device at the path cannot make the wrapper
    // wait for a writer before it has even started a worker.
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (None, None),
        Err(error) => {
            return (
                None,
                Some(skipped_brief(&format!(
                    "cannot read {}: {error}",
                    path.display()
                ))),
            );
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            return (
                None,
                Some(skipped_brief(&format!(
                    "cannot read {}: {error}",
                    path.display()
                ))),
            );
        }
    };
    // Only a regular file is read: a FIFO, a directory or a device is skipped with a note.
    if !metadata.is_file() {
        return (
            None,
            Some(skipped_brief(&format!(
                "{} is not a regular file",
                path.display()
            ))),
        );
    }
    let len = metadata.len();
    if len > WORKER_BRIEF_LIMIT {
        return (
            None,
            Some(skipped_brief(&format!(
                "{len} bytes, the limit is {WORKER_BRIEF_LIMIT} bytes"
            ))),
        );
    }
    // The handle, not the path, is read, and at most one byte past the limit: a file that grew
    // after the check above cannot make the wrapper read more than the limit.
    let mut bytes = Vec::new();
    if let Err(error) = (&file).take(WORKER_BRIEF_LIMIT + 1).read_to_end(&mut bytes) {
        return (
            None,
            Some(skipped_brief(&format!(
                "cannot read {}: {error}",
                path.display()
            ))),
        );
    }
    if bytes.len() as u64 > WORKER_BRIEF_LIMIT {
        return (
            None,
            Some(skipped_brief(&format!(
                "{} bytes, the limit is {WORKER_BRIEF_LIMIT} bytes",
                bytes.len()
            ))),
        );
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return (None, Some(skipped_brief("not UTF-8")));
    };
    if text.trim().is_empty() {
        return (None, None);
    }
    (Some(text), None)
}

/// The one line the attempt record and `show` carry for a skipped worker brief.
fn skipped_brief(reason: &str) -> String {
    format!("{} skipped: {reason}", crate::store::WORKER_BRIEF_FILE)
}

/// The brief the worker receives, in order: the `merge-main` conflict section, a fresh
/// rework's hand-off, then the brief arc handed over, then the repository's standing worker
/// instructions. An empty section takes no room.
fn compose_brief(sections: &[Option<&str>]) -> String {
    sections
        .iter()
        .flatten()
        .map(|section| section.trim())
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The largest file a brief may point at under `runs/`; a larger one is skipped (ADR 0048
/// decision 7).
const VERDICT_COPY_LIMIT: u64 = 1024 * 1024;

/// The directory in the worker's worktree that holds the copies (ADR 0048 decision 7).
const VERDICT_COPY_DIR: &str = ".radian/arc-verdicts";

/// Copies every attempt file a brief names under `arc/runs/` into the worker's worktree and
/// rewrites its path to the copy (ADR 0048 decision 7). A path that cannot be copied stays in
/// the text as it is and gets one note line for the attempt record; nothing here fails the
/// attempt. The copy is keyed by the path's own `runs/<task>/attempts/<attempt>/` segments, so
/// a follow-up task can name another task's verdict.
fn copy_verdicts(brief: &str, arc: &Path, worktree: &Path) -> (String, Vec<String>) {
    let runs = arc.join("runs");
    let prefix = format!("{}/", runs.display());
    let mut text = String::with_capacity(brief.len());
    let mut notes = Vec::new();
    let mut rest = brief;
    while let Some(start) = rest.find(&prefix) {
        text.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .char_indices()
            .find(|(_, c)| ends_path(*c))
            .map(|(index, _)| index)
            .unwrap_or(tail.len());
        let named = &tail[..end];
        match copy_verdict(named, &runs, worktree) {
            Ok(relative) => text.push_str(&relative.display().to_string()),
            Err(reason) => {
                text.push_str(named);
                notes.push(format!("verdict copy skipped: {named}: {reason}"));
            }
        }
        rest = &tail[end..];
    }
    text.push_str(rest);
    (text, notes)
}

/// Whether `c` ends a path in a brief. The briefs wrap a path in backticks; prose around one
/// may add a comma, a bracket or a quote.
fn ends_path(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '`' | ',' | ';' | '\'' | '"' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
}

/// Copies one named attempt file into the worktree and returns the path the worker reads. The
/// reason a copy is refused is the second half of the note line.
fn copy_verdict(named: &str, runs: &Path, worktree: &Path) -> Result<PathBuf, String> {
    let path = Path::new(named);
    let Some((task, attempt)) = verdict_ids(path, runs) else {
        return Err("not a file of an attempt".to_owned());
    };
    let file = path
        .file_name()
        .ok_or_else(|| "it names no file".to_owned())?;
    let relative = Path::new(VERDICT_COPY_DIR)
        .join(&task)
        .join(&attempt)
        .join(file);
    let mut source = open_verdict(path, runs)?;
    // One handle for the checks and the read: a path swapped after the open cannot change
    // what is copied (ADR 0048 decision 7).
    let metadata = source
        .metadata()
        .map_err(|error| format!("cannot read it: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".to_owned());
    }
    if metadata.len() > VERDICT_COPY_LIMIT {
        return Err(format!(
            "{} bytes, the limit is {VERDICT_COPY_LIMIT} bytes",
            metadata.len()
        ));
    }
    let mut bytes = Vec::new();
    (&mut source)
        .take(VERDICT_COPY_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read it: {error}"))?;
    if bytes.len() as u64 > VERDICT_COPY_LIMIT {
        return Err(format!(
            "{} bytes, the limit is {VERDICT_COPY_LIMIT} bytes",
            bytes.len()
        ));
    }
    write_verdict_copy(worktree, &relative, &bytes)?;
    Ok(relative)
}

/// Opens a named attempt file without following a symbolic link. A worker can plant a link in
/// its own worktree; a link where a verdict should be is not the regular file the brief
/// promises, so the path stays in the text with a note (ADR 0048 decision 7).
fn open_verdict(path: &Path, runs: &Path) -> Result<File, String> {
    let relative = path
        .strip_prefix(runs)
        .map_err(|_| "not a file of an attempt".to_owned())?;
    let count = relative.components().count();
    let mut current = runs.to_path_buf();
    for (index, component) in relative.components().enumerate() {
        current.push(component.as_os_str());
        let metadata =
            fs::symlink_metadata(&current).map_err(|error| format!("cannot read it: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("it is a symbolic link".to_owned());
        }
        if index + 1 < count {
            if !metadata.is_dir() {
                return Err("a component of it is not a directory".to_owned());
            }
        } else if !metadata.is_file() {
            return Err("not a regular file".to_owned());
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // The final open does not follow a link either, so a check-then-open swap cannot
        // hand the copy a different file.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|error| format!("cannot read it: {error}"))
}

/// Writes a verdict copy under `worktree` without following a symbolic link at any component.
/// The worker controls its worktree between attempts: it can plant a link at `.radian/` or at a
/// copy path, and a process it left behind can rename a directory the wrapper already looked at
/// and put a link in its place. On unix every directory is therefore opened once, without
/// following a link, and descended by handle, so a name swapped after a directory is created or
/// checked cannot redirect the write (ADR 0048 decision 7).
fn write_verdict_copy(worktree: &Path, relative: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        write_verdict_copy_at(worktree, relative, bytes)
    }
    #[cfg(not(unix))]
    {
        write_verdict_copy_by_path(worktree, relative, bytes)
    }
}

/// The descriptor-relative form of [`write_verdict_copy`]: no copy path is resolved twice
/// (ADR 0048 decision 7).
#[cfg(unix)]
fn write_verdict_copy_at(worktree: &Path, relative: &Path, bytes: &[u8]) -> Result<(), String> {
    let components = relative
        .components()
        .map(|component| component.as_os_str())
        .collect::<Vec<_>>();
    let (file, parents) = components
        .split_last()
        .ok_or_else(|| "the copy names no file".to_owned())?;
    let mut shown = worktree.to_path_buf();
    let mut dir = open_dir(worktree)
        .map_err(|error| format!("cannot open {}: {error}", worktree.display()))?;
    for parent in parents {
        shown.push(parent);
        let name = c_name(parent, &shown)?;
        dir = open_or_create_dir(&dir, &name, &shown)?;
    }
    shown.push(file);
    let name = c_name(file, &shown)?;
    write_file_at(&dir, &name, &shown, bytes)
}

/// The path-based form of [`write_verdict_copy`] for a platform without `openat`. It refuses a
/// link at each component but, unlike the unix form, resolves a name a second time after the
/// check (ADR 0048 decision 7).
#[cfg(not(unix))]
fn write_verdict_copy_by_path(
    worktree: &Path,
    relative: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    let components = relative.components().collect::<Vec<_>>();
    let mut current = worktree.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        if index + 1 == components.len() {
            write_file_without_link(&current, bytes)?;
        } else {
            create_dir_without_link(&current)?;
        }
    }
    Ok(())
}

/// A component's name as a C string for the `*at` calls.
#[cfg(unix)]
fn c_name(name: &std::ffi::OsStr, shown: &Path) -> Result<std::ffi::CString, String> {
    use std::os::unix::ffi::OsStrExt as _;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| format!("{} has a null byte in a component", shown.display()))
}

/// Opens a directory without following a link at its final component.
#[cfg(unix)]
fn open_dir(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    options.open(path)
}

/// Opens the directory `name` in `dir`, creating it first when it is missing. Both calls resolve
/// `name` against the held parent handle, so a directory renamed and replaced by a link after the
/// create is not followed (ADR 0048 decision 7).
#[cfg(unix)]
fn open_or_create_dir(dir: &File, name: &std::ffi::CStr, shown: &Path) -> Result<File, String> {
    use std::os::unix::io::AsRawFd as _;

    let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o777) };
    if rc == 0 {
        #[cfg(test)]
        after_mkdir(shown);
    } else {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(format!("cannot create {}: {error}", shown.display()));
        }
    }
    open_dir_at(dir, name, shown)
}

/// Opens `name` in `dir` as a directory, following no link.
#[cfg(unix)]
fn open_dir_at(dir: &File, name: &std::ffi::CStr, shown: &Path) -> Result<File, String> {
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd >= 0 {
        // SAFETY: `openat` returned this descriptor, it is open, and this File owns it.
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    let error = std::io::Error::last_os_error();
    Err(format!(
        "{} {}",
        shown.display(),
        component_reason(dir, name, libc::S_IFDIR, "not a directory", &error)
    ))
}

/// Writes `bytes` to `name` in `dir`, creating or truncating the file without following a link.
/// A later attempt overwrites the copy of an earlier one.
#[cfg(unix)]
fn write_file_at(
    dir: &File,
    name: &std::ffi::CStr,
    shown: &Path,
    bytes: &[u8],
) -> Result<(), String> {
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};

    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o644) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!(
            "{} {}",
            shown.display(),
            component_reason(dir, name, libc::S_IFREG, "not a regular file", &error)
        ));
    }
    // SAFETY: `openat` returned this descriptor, it is open, and this File owns it.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .map_err(|error| format!("cannot write {}: {error}", shown.display()))
}

/// Why opening `name` in `dir` failed, for the note line. A link and a file of the wrong type are
/// named as such; anything else keeps the OS error. `fstatat` resolves the name against the same
/// held handle, so the answer describes what the wrapper just tried to open.
#[cfg(unix)]
fn component_reason(
    dir: &File,
    name: &std::ffi::CStr,
    expected: libc::mode_t,
    wrong: &str,
    error: &std::io::Error,
) -> String {
    use std::os::unix::io::AsRawFd as _;

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        let kind = stat.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            return "is a symbolic link".to_owned();
        }
        if kind != expected {
            return format!("is {wrong}");
        }
    }
    format!("cannot be opened: {error}")
}

#[cfg(test)]
type MkdirHook = Box<dyn Fn(&Path)>;

// The test-only seam between creating a copy directory and opening it by handle. The unit test
// `tests::a_verdict_copy_survives_a_directory_swapped_for_a_link` uses it to rename the new
// directory and put a symbolic link in its place, the window a path-based traversal leaves open.
// Nothing sets it outside a test, so it does nothing in a real run.
#[cfg(test)]
thread_local! {
    static AFTER_MKDIR: std::cell::RefCell<Option<MkdirHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Calls the hook [`AFTER_MKDIR`] once, when a test set one.
#[cfg(test)]
fn after_mkdir(path: &Path) {
    if let Some(hook) = AFTER_MKDIR.with(|hook| hook.borrow_mut().take()) {
        hook(path);
    }
}

/// Creates `path` as a directory unless it is one already, and refuses a symbolic link or a
/// non-directory at it.
#[cfg(not(unix))]
fn create_dir_without_link(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("{} is a symbolic link", path.display()))
        }
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!("{} is not a directory", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                create_dir_without_link(path)
            }
            Err(error) => Err(format!("cannot create {}: {error}", path.display())),
        },
        Err(error) => Err(format!("cannot read {}: {error}", path.display())),
    }
}

/// Replaces the file at `path` with `bytes`, and refuses a symbolic link or a non-regular file
/// already at it. A later attempt overwrites the copy of an earlier one.
#[cfg(not(unix))]
fn write_file_without_link(path: &Path, bytes: &[u8]) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!("{} is a symbolic link", path.display()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(format!("{} is not a regular file", path.display()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

/// The `runs/<task>/attempts/<attempt>/…` ids a named path carries, when it has them. A path
/// that walks out of `runs/` is not one of its files, whatever it names.
fn verdict_ids(path: &Path, runs: &Path) -> Option<(String, String)> {
    let relative = path.strip_prefix(runs).ok()?;
    if !relative
        .components()
        .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let mut parts = relative.iter();
    let task = parts.next()?.to_str()?.to_owned();
    if parts.next()?.to_str()? != "attempts" {
        return None;
    }
    let attempt = parts.next()?.to_str()?.to_owned();
    parts.next()?;
    Some((task, attempt))
}

/// The section a fresh rework gets in front of the brief (ADR 0048 decision 7): the previous
/// attempt's final report, the commits of the task's own change, and its diff stat. `None` on
/// the first attempt and for a `resume` attempt, whose session already holds the earlier
/// turns. The worker can read the files, so the section hands over the end and points at the
/// branch rather than repeating the conversation.
fn handoff_section(
    o: &Options,
    s: &Setup,
    head: &str,
    base_sha: &str,
) -> Result<Option<String>, Failure> {
    if o.attempt <= 1 || o.resume {
        return Ok(None);
    }
    let main = format!("refs/remotes/{}/{}", o.remote, o.main_branch);
    // The task's own change, from the merge base with the remote target branch: a merge of the
    // target brings its files in and is not the task's change (ADR 0048 decision 8). The
    // recorded base covers a repository whose target ref cannot be read.
    let own_base =
        merge_base(&s.repo, &main, head, &s.clock)?.unwrap_or_else(|| base_sha.to_owned());
    let range = format!("{own_base}..{head}");
    let commits = git(&s.repo, &["log", "--oneline", &range], &s.clock)?;
    let stat = git(&s.repo, &["diff", "--stat", &range], &s.clock)?;
    let previous = o.attempt - 1;
    // A worker-less attempt (a `reverify`) writes no `result.jsonl`, and those attempts are
    // claimed by the loop by itself, so the attempt that just ended is often not the one with
    // the report: the hand-off carries the newest one (ADR 0048 decision 7).
    let report = match newest_report(&s.arc, &o.task, o.attempt) {
        Some((from, report)) => {
            let report = cap_report(&report);
            if from == previous {
                report
            } else {
                format!(
                    "Attempt {from} is the newest attempt that wrote a final report; attempt \
                     {previous} ended without one.\n\n{report}"
                )
            }
        }
        None => "The previous attempt wrote no final report: it ended without a result.".to_owned(),
    };
    let commits = or_placeholder(&commits, "(no commits of its own yet)");
    let stat = or_placeholder(&stat, "(no file changes yet)");
    Ok(Some(format!(
        "## Where the previous attempt left off\n\n\
         Attempt {previous} of this task ended, and this is a new session: its conversation is \
         not carried over. The full history is on the branch `{}`; read the files you need \
         rather than assume what the earlier attempt did.\n\n\
         ### The previous attempt's final report\n\n\
         {report}\n\n\
         ### The commits on the branch\n\n\
         ```text\n{commits}\n```\n\n\
         ### The change so far\n\n\
         ```text\n{stat}\n```",
        s.branch,
    )))
}

/// `text` trimmed, or `placeholder` when it holds nothing.
fn or_placeholder(text: &str, placeholder: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        placeholder.to_owned()
    } else {
        text.to_owned()
    }
}

/// The newest earlier attempt that wrote a final report: its number and the report. A
/// worker-less attempt (a `reverify`) leaves no `result.jsonl`, and the scheduling loop claims
/// those by itself, so the attempt before a rework is often one. An attempt before the first
/// has nothing to offer.
fn newest_report(arc: &Path, task: &str, attempt: u32) -> Option<(u32, String)> {
    (1..attempt)
        .rev()
        .find_map(|earlier| report_of(arc, task, earlier).map(|report| (earlier, report)))
}

/// The worker's last assistant message of one attempt, from its `result.jsonl`.
fn report_of(arc: &Path, task: &str, attempt: u32) -> Option<String> {
    let path = arc
        .join("runs")
        .join(task)
        .join("attempts")
        .join(format!("{task}-{attempt}"))
        .join("result.jsonl");
    let text = last_result(&path)?.get("text")?.as_str()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The report cut at [`HANDOFF_REPORT_CHARS`] characters, with a note when it was cut.
fn cap_report(report: &str) -> String {
    if report.chars().count() <= HANDOFF_REPORT_CHARS {
        return report.to_owned();
    }
    let mut cut: String = report.chars().take(HANDOFF_REPORT_CHARS).collect();
    cut.push_str("\n\n[... the previous attempt's report is cut at 8,000 characters ...]");
    cut
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
            notes: &[],
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

/// Whether the worker's session directory holds `<session>.jsonl`, the file `radian exec
/// --resume <session>` opens. The directory is the one the worker resolves: a `--session-dir`
/// among the extra arguments, else `RADIAN_SESSION_DIR`, else the user's data directory.
fn session_exists(session: &str, extra_args: &[String]) -> bool {
    let flag = session_dir_arg(extra_args);
    radian_host::session_dir(flag.as_deref())
        .is_ok_and(|dir| dir.join(format!("{session}.jsonl")).is_file())
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

/// The worker's session flags for one attempt (ADR 0048 decisions 4 and 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionStart {
    /// Start a new session: `--session-id <session>`.
    New,
    /// Continue the session: `--resume <session>`.
    Continue,
}

impl SessionStart {
    /// The value a command worker receives in `ARC_SESSION_START`.
    fn as_str(self) -> &'static str {
        match self {
            SessionStart::New => "new",
            SessionStart::Continue => "resume",
        }
    }
}

/// What this attempt does with its session. A `resume`-mode later attempt continues the
/// session only when its file exists: an earlier attempt that failed before the model started
/// left none, and the next one then starts it. A session file alone never resumes, and a
/// `fresh` attempt never does.
fn session_start(resume: bool, exists: bool) -> SessionStart {
    if resume && exists {
        SessionStart::Continue
    } else {
        SessionStart::New
    }
}

/// The environment a command worker or ACP agent receives (ADR 0048 decision 7). A program
/// that contains a `/` already arrives absolute (the claim resolves it); the worktree, the git
/// directories and the state directory are made absolute here.
fn worker_env(
    o: &Options,
    worktree: &Path,
    deny: &[PathBuf],
    state_dir: &Path,
    control: &GitControl,
    start: SessionStart,
) -> Vec<(&'static str, String)> {
    let absolute = |path: &Path| {
        fs::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .display()
            .to_string()
    };
    let deny: Vec<String> = deny.iter().map(|path| path.display().to_string()).collect();
    vec![
        ("ARC_WORKTREE", absolute(worktree)),
        ("ARC_TASK", o.task.clone()),
        ("ARC_ATTEMPT", format!("{}-{}", o.task, o.attempt)),
        ("ARC_SESSION", o.session.clone()),
        ("ARC_SESSION_START", start.as_str().to_owned()),
        ("ARC_GIT_DIR", absolute(&control.git_dir)),
        ("ARC_GIT_COMMON_DIR", absolute(&control.common)),
        ("ARC_DENY_READ", deny.join(":")),
        ("ARC_STATE_DIR", absolute(state_dir)),
    ]
}

/// What one spawn needs: the paths of the attempt, the record of its git control files, and
/// the session decision the attempt made once.
struct Spawn<'a> {
    worktree: &'a Path,
    deny: &'a [PathBuf],
    state_dir: &'a Path,
    control: &'a GitControl,
    result_path: &'a Path,
    stderr_path: &'a Path,
    brief: &'a str,
    /// What the worker does with its session (ADR 0048 decision 4). Computed once, so the
    /// flags and the protocol request can never disagree.
    start: SessionStart,
}

fn spawn_child(o: &Options, spawn: &Spawn<'_>) -> Result<Child, Failure> {
    let Spawn {
        worktree,
        deny,
        state_dir,
        control,
        result_path,
        stderr_path,
        brief,
        start,
    } = *spawn;
    let mut command = match worker_of(o) {
        Worker::Shell(shell) => {
            // Test-only (see [`Options::worker_command`]). `sh` starts in the wrapper's own
            // working directory and its background children stay in its process group, so
            // the group signal this module forwards can be observed.
            let mut command = Command::new("sh");
            command.arg("-c").arg(shell);
            command
        }
        Worker::Command(words) => {
            let Some((program, args)) = words.split_first() else {
                return Err(Failure::usage(
                    "the configured worker command is empty; its first element must be the \
                     program",
                ));
            };
            let mut command = Command::new(program);
            command.args(args);
            // The command works in the task's worktree, as `radian exec` does (decision 7).
            command.current_dir(worktree);
            for (name, value) in worker_env(o, worktree, deny, state_dir, control, start) {
                command.env(name, value);
            }
            command
        }
        // An ACP agent (a flow's `acp` or `radian acp`) is spawned by [`spawn_acp`], which
        // pipes its stdout to the client.
        Worker::Acp(_) | Worker::Radian => {
            return Err(Failure::usage(
                "the ACP worker is started by the wrapper's ACP client, not here",
            ));
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

/// Spawns one ACP agent: the flow's program in the task's worktree, in its own process group,
/// with the command worker's environment and stderr stream (ADR 0048 decision 7). Its stdout
/// is piped to the ACP client, and the attempt's `result.jsonl` is created here so the client
/// writes the protocol's lines into it.
fn spawn_acp(
    o: &Options,
    spawn: &Spawn<'_>,
    words: &[String],
) -> Result<(Child, acp::Log), Failure> {
    let Spawn {
        worktree,
        deny,
        state_dir,
        control,
        start,
        ..
    } = *spawn;
    let Some((program, args)) = words.split_first() else {
        return Err(Failure::usage(
            "the configured ACP agent is empty; its first element must be the program",
        ));
    };
    let mut command = Command::new(program);
    command.args(args).current_dir(worktree);
    for (name, value) in worker_env(o, worktree, deny, state_dir, control, start) {
        command.env(name, value);
    }
    spawn_agent(command, spawn)
}

/// Spawns the `radian acp` the default worker runs (ADR 0048 decision 7): the same flag string
/// the `radian exec` worker got, without `-p -`, `--output jsonl` and `--yes`. The worktree
/// goes in the session request's `cwd` and in `--workspace`, so the two agree.
fn spawn_radian_acp(o: &Options, spawn: &Spawn<'_>) -> Result<(Child, acp::Log), Failure> {
    let command = radian_acp_command(o, spawn.worktree, spawn.deny, spawn.start)?;
    spawn_agent(command, spawn)
}

/// The `radian acp` command line of one attempt. `--session-id` and `--resume` are exclusive
/// (ADR 0022/0023): the session is the one the claim chose, and a `fresh` attempt is never
/// resumed. Which one is passed also decides `session/new` versus `session/load`, so both
/// read the one [`SessionStart`] the attempt made.
fn radian_acp_command(
    o: &Options,
    worktree: &Path,
    deny: &[PathBuf],
    start: SessionStart,
) -> Result<Command, Failure> {
    let executable = std::env::current_exe()
        .map_err(|error| Failure::error(format!("cannot find the current executable: {error}")))?;
    let mut command = Command::new(executable);
    command.arg("acp");
    match start {
        SessionStart::Continue => {
            command.arg("--resume").arg(&o.session);
        }
        SessionStart::New => {
            command.arg("--session-id").arg(&o.session);
        }
    }
    command
        .arg("--workspace")
        .arg(worktree)
        .arg("--profile")
        .arg("workspace");
    for path in deny {
        command.arg("--deny-read").arg(path);
    }
    command.args(&o.extra_args);
    Ok(command)
}

/// The last steps every ACP agent gets: its own process group, so the wrapper's SIGTERM
/// reaches the agent and every child that stays in the group and never this wrapper or the
/// process above it (ADR 0048 decision 9); stdout piped to the client; stderr into
/// `stderr.log`; and `result.jsonl` created for the client's lines.
fn spawn_agent(mut command: Command, spawn: &Spawn<'_>) -> Result<(Child, acp::Log), Failure> {
    let Spawn {
        result_path,
        stderr_path,
        ..
    } = *spawn;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let log_file = File::create(result_path).map_err(|error| {
        Failure::error(format!("cannot create {}: {error}", result_path.display()))
    })?;
    let stderr = File::create(stderr_path).map_err(|error| {
        Failure::error(format!("cannot create {}: {error}", stderr_path.display()))
    })?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr));
    let child = command
        .spawn()
        .map_err(|error| Failure::error(format!("cannot start the worker: {error}")))?;
    Ok((child, Arc::new(std::sync::Mutex::new(log_file))))
}

/// How long an ACP agent gets to end its turn after `session/cancel` before the wrapper sends
/// SIGTERM. The attempt's own `cancel_grace_secs` starts with that signal, not before it
/// (ADR 0048 decision 9).
const CANCEL_TURN_WAIT: Duration = Duration::from_secs(2);

/// What one ACP turn left: the process exit code and any protocol-level failure text.
struct AcpTurn {
    exit_code: i32,
    failure: Option<String>,
    /// The turn's end, when the agent answered `session/prompt`. The `Result` line is written
    /// after the client has stopped, so it is the attempt's last line.
    end: Option<acp::PromptEnd>,
}

/// How long the agent gets to exit on its own after the wrapper closes its stdin at the end of
/// a turn, before the wrapper signals its process group (`radian acp` exits `0` on the end of
/// its protocol stream). This wait is not part of the attempt's limits; a signal ends it.
const AGENT_EXIT_WAIT: Duration = Duration::from_secs(5);

/// Runs the one conversation of an ACP attempt, then stops the agent and writes the attempt's
/// `Result` line (ADR 0048 decision 7): `initialize`, one `session/new` (or, for a `resume`
/// `radian` attempt whose session exists, `session/load`) in the worktree, and one
/// `session/prompt` with the brief. The response's `stopReason` decides the outcome; on a
/// stall, the wall clock or a cancel the client sends `session/cancel` first.
fn run_acp_turn(
    o: &Options,
    s: &Setup,
    child: &mut Child,
    client: &mut acp::Client,
    brief: &str,
    log: &acp::Log,
    start: SessionStart,
) -> Result<AcpTurn, Failure> {
    let limits = Limits::of(o);
    let turn = match acp_conversation(
        o,
        s,
        child,
        client,
        brief,
        &s.runs.join(SIGNAL_FILE),
        &limits,
        start,
    ) {
        Ok(turn) => Ok(turn),
        // A request that could not be written means the agent's pipe is gone. When the agent
        // itself has exited, its exit code is the attempt's, exactly as the old jsonl monitor
        // recorded a killed worker; otherwise stop whatever is left of its group so nothing of
        // the worker outlives the attempt.
        Err(failure) => match exited(child) {
            Ok(Some(status)) => {
                stop_group(child.id(), limits.grace);
                let _ = child.wait();
                Ok(AcpTurn {
                    exit_code: exit_code_of(&status),
                    failure: None,
                    end: None,
                })
            }
            _ => {
                stop_agent(child, limits.grace);
                Err(failure)
            }
        },
    };
    client.finish();
    // The wrapper, not the agent, writes the attempt's `Result` line, after the agent has ended
    // and the reader has stopped, so nothing follows it in `result.jsonl` (ADR 0048 decision 7).
    if let Ok(turn) = &turn
        && let Some(end) = &turn.end
    {
        acp::append(log, &result_line(o, end));
    }
    turn
}

/// The attempt's `Result` line (ADR 0048 decisions 7 and 12). For a flow's `acp` agent it is
/// the `stopReason` mapping and the last message run; for the `radian` worker it is what
/// `radian acp` puts in `_meta.radian`: `text` is the last assistant message, and `cost` is the
/// answering turn's fee in the `{micros, incomplete, estimated}` shape `radian exec` writes
/// (ADR 0049), so the task's cost, `cost_unknown` and the budget checks can read the attempt's
/// own cost from it.
fn result_line(o: &Options, end: &acp::PromptEnd) -> Value {
    let mut result = json!({
        "type": "Result",
        "outcome": acp_outcome(&end.stop_reason),
        "text": end.text,
    });
    if matches!(worker_of(o), Worker::Radian) {
        apply_radian_meta(&mut result, &end.meta);
    }
    result
}

/// `_meta.radian` on the attempt's `Result`: `text` is the last assistant message in full, and
/// `cost` is the answering turn's fee (ADR 0049), null included. A `_meta` without `radian`
/// leaves the line as the protocol's own stop reason gave it.
fn apply_radian_meta(result: &mut Value, meta: &Value) {
    let Some(radian) = meta.get("radian") else {
        return;
    };
    if let Some(text) = radian.get("text").and_then(Value::as_str) {
        result["text"] = json!(text);
    }
    if let Some(cost) = radian.get("cost") {
        result["cost"] = cost.clone();
    }
}

#[allow(clippy::too_many_arguments)]
fn acp_conversation(
    o: &Options,
    s: &Setup,
    child: &mut Child,
    client: &acp::Client,
    brief: &str,
    signal: &Path,
    limits: &Limits,
    start: SessionStart,
) -> Result<AcpTurn, Failure> {
    // No client capability is advertised: the agent writes its own files and runs no shell
    // through this client, and it never asks `fs/*` or `terminal/*` (ADR 0048 decision 7).
    let initialized = client
        .request(
            "initialize",
            json!({
                "protocolVersion": acp::PROTOCOL_VERSION,
                "clientCapabilities": {},
            }),
        )
        .map_err(|error| Failure::error(format!("initialize: {error}")))?;
    match wait_acp(child, client, signal, limits, s.clock.deadline(), || {
        client.has_response(initialized)
    })? {
        Wait::Done => {}
        Wait::Stopped { exit_code, .. } | Wait::Exited { exit_code } => {
            return Ok(AcpTurn {
                exit_code,
                failure: None,
                end: None,
            });
        }
    }
    if let Some(Err(error)) = client.take_response(initialized) {
        return Ok(AcpTurn {
            exit_code: stop_agent(child, limits.grace).map_or(-1, |status| exit_code_of(&status)),
            failure: Some(format!("initialize failed: {error}")),
            end: None,
        });
    }

    // One session per process. A `resume`-mode later attempt of the `radian` worker opens the
    // task's session with `session/load`, which continues it (ADR 0048 decisions 4 and 7); the
    // agent was started with `--resume`, so the id agrees. Every other attempt, and every
    // flow's own ACP agent, starts a session with `session/new`.
    let resume_session = matches!(worker_of(o), Worker::Radian) && start == SessionStart::Continue;
    let cwd = fs::canonicalize(&s.worktree)
        .unwrap_or_else(|_| s.worktree.clone())
        .display()
        .to_string();
    let (method, params) = if resume_session {
        (
            "session/load",
            json!({ "sessionId": o.session, "cwd": cwd, "mcpServers": [] }),
        )
    } else {
        ("session/new", json!({ "cwd": cwd, "mcpServers": [] }))
    };
    let created = client
        .request(method, params)
        .map_err(|error| Failure::error(format!("{method}: {error}")))?;
    match wait_acp(child, client, signal, limits, s.clock.deadline(), || {
        client.has_response(created)
    })? {
        Wait::Done => {}
        Wait::Stopped { exit_code, .. } | Wait::Exited { exit_code } => {
            return Ok(AcpTurn {
                exit_code,
                failure: None,
                end: None,
            });
        }
    }
    let session = match client.take_response(created) {
        // `session/load` answers with an empty object and keeps the requested id.
        Some(Ok(_)) if resume_session => Some(o.session.clone()),
        Some(Ok(value)) => value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some(Err(error)) => {
            return Ok(AcpTurn {
                exit_code: stop_agent(child, limits.grace)
                    .map_or(-1, |status| exit_code_of(&status)),
                failure: Some(format!("{method} failed: {error}")),
                end: None,
            });
        }
        None => None,
    };
    let Some(session) = session else {
        return Ok(AcpTurn {
            exit_code: stop_agent(child, limits.grace).map_or(-1, |status| exit_code_of(&status)),
            failure: Some(format!("{method} answered without a sessionId")),
            end: None,
        });
    };
    client.set_session(&session);

    let prompted = client
        .request(
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": brief }],
            }),
        )
        .map_err(|error| Failure::error(format!("session/prompt: {error}")))?;
    let stopped = wait_acp(child, client, signal, limits, s.clock.deadline(), || {
        client.has_response(prompted)
    })?;
    let response = client.take_response(prompted);
    let (exit_code, end) = match stopped {
        // The turn is over: the wrapper closes the agent's stdin, which is the protocol
        // stream's end, and gives it a moment to exit before it signals the group. That is
        // what lets `radian acp` shut down and exit `0` (ADR 0048 decision 7).
        Wait::Done => (
            end_agent(child, client, limits.grace),
            prompt_end(response, client),
        ),
        // `session/cancel` asked the agent to end the turn. If it answered `cancelled` in the
        // moment before SIGTERM, that answer is the attempt's result; otherwise the wrapper
        // wrote no result and the signal records the cancel (ADR 0048 decision 9).
        Wait::Stopped {
            reason: "cancel",
            exit_code,
        } => {
            let cancelled = response
                .as_ref()
                .and_then(|response| response.as_ref().ok())
                .and_then(|value| value.get("stopReason"))
                .and_then(Value::as_str)
                == Some("cancelled");
            if cancelled {
                (exit_code, prompt_end(response, client))
            } else {
                return Ok(AcpTurn {
                    exit_code,
                    failure: None,
                    end: None,
                });
            }
        }
        Wait::Stopped { exit_code, .. } | Wait::Exited { exit_code } => {
            return Ok(AcpTurn {
                exit_code,
                failure: None,
                end: None,
            });
        }
    };
    let (end, failure) = match end {
        Ok(end) => (Some(end), None),
        Err(error) => (None, Some(error)),
    };
    Ok(AcpTurn {
        exit_code,
        failure,
        end,
    })
}

/// The prompt response as a [`acp::PromptEnd`], or the line the attempt fails with when the
/// agent answered with an error or without a `stopReason`.
fn prompt_end(
    response: Option<Result<Value, String>>,
    client: &acp::Client,
) -> Result<acp::PromptEnd, String> {
    match response {
        Some(Ok(value)) => match value.get("stopReason").and_then(Value::as_str) {
            Some(stop_reason) => Ok(acp::PromptEnd {
                stop_reason: stop_reason.to_owned(),
                text: client.message_text(),
                meta: value.get("_meta").cloned().unwrap_or(Value::Null),
            }),
            None => Err("the agent's session/prompt answer carries no stopReason".to_owned()),
        },
        Some(Err(error)) => Err(format!("session/prompt failed: {error}")),
        None => Err("the agent exited without answering session/prompt".to_owned()),
    }
}

/// The outcome a `stopReason` records (ADR 0048 decisions 7 and 9): `end_turn` is the only
/// pass; every other protocol reason is a failed outcome named after itself, and a reason this
/// build does not know is `failed`.
fn acp_outcome(stop_reason: &str) -> &'static str {
    match stop_reason {
        "end_turn" => "completed",
        "max_tokens" => "max_tokens",
        "max_turn_requests" => "max_turn_requests",
        "refusal" => "refusal",
        "cancelled" => "cancelled",
        _ => "failed",
    }
}

/// One step's wait: the response arrived, the attempt was stopped, or the agent exited.
enum Wait {
    Done,
    Stopped {
        reason: &'static str,
        exit_code: i32,
    },
    Exited {
        exit_code: i32,
    },
}

/// Waits for one ACP step while enforcing the attempt's limits (ADR 0048 decisions 7.8 and 9).
/// `done` is the step's response. No new protocol line for the stall window, the wall clock,
/// or a `radian arc cancel` (or a SIGTERM to the wrapper) sends `session/cancel`, gives the
/// agent a moment to end its turn, and then sends SIGTERM, waits the grace period and sends
/// SIGKILL. The wall clock and a stall also record `signal.json`; a cancel's record was
/// written by the caller that signalled the wrapper. An agent that exits on its own is reaped
/// here too.
fn wait_acp(
    child: &mut Child,
    client: &acp::Client,
    signal_path: &Path,
    limits: &Limits,
    wall_deadline: Option<Instant>,
    done: impl Fn() -> bool,
) -> Result<Wait, Failure> {
    let mut last_growth = Instant::now();
    let mut last_activity = client.activity();
    loop {
        if done() {
            return Ok(Wait::Done);
        }
        if let Some(status) = exited(child)? {
            stop_group(child.id(), limits.grace);
            let _ = child.wait();
            return Ok(Wait::Exited {
                exit_code: exit_code_of(&status),
            });
        }
        let now = Instant::now();
        let activity = client.activity();
        if activity != last_activity {
            last_activity = activity;
            last_growth = now;
        }
        // The order of the command worker's monitor: a cancel from another process first,
        // then the wall clock, then the stall timer. A SIGTERM sent to the wrapper alone, with
        // no `signal.json`, is a stop too: the ACP handler does not forward it, so the protocol
        // cancel below is the only way the agent learns of it.
        let (reason, record) = if signal_path.is_file() || STOP_REQUESTED.load(Ordering::SeqCst) {
            (Some("cancel"), false)
        } else if wall_deadline.is_some_and(|deadline| now >= deadline) {
            (Some("wall_time"), true)
        } else if !limits.stall.is_zero() && now.duration_since(last_growth) >= limits.stall {
            (Some("stall"), true)
        } else {
            (None, false)
        };
        let Some(reason) = reason else {
            std::thread::sleep(POLL);
            continue;
        };
        if record {
            write_signal(signal_path, reason)?;
        }
        // The protocol's own stop first: the agent ends the turn, and the reader answers a
        // pending permission request `cancelled` from here on.
        client.cancel();
        let turn_ends = Instant::now() + CANCEL_TURN_WAIT;
        while Instant::now() < turn_ends {
            if done() || exited(child)?.is_some() {
                break;
            }
            std::thread::sleep(POLL);
        }
        process::signal_group(child.id(), process::SIGTERM);
        let kill_at = Instant::now() + limits.grace;
        loop {
            if exited(child)?.is_some() {
                break;
            }
            if Instant::now() >= kill_at {
                process::signal_group(child.id(), process::SIGKILL);
                break;
            }
            std::thread::sleep(POLL);
        }
        let status = child
            .wait()
            .map_err(|error| Failure::error(format!("cannot wait for the agent: {error}")))?;
        return Ok(Wait::Stopped {
            reason,
            exit_code: exit_code_of(&status),
        });
    }
}

/// The agent's exit status without reaping it.
fn exited(child: &mut Child) -> Result<Option<ExitStatus>, Failure> {
    process::exited_status(child.id())
        .map_err(|error| Failure::error(format!("cannot wait for the agent: {error}")))
}

/// Stops the agent's process group and reaps the child. `None` when the status cannot be read
/// (the child was already reaped elsewhere).
fn stop_agent(child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    stop_group(child.id(), grace);
    child.wait().ok()
}

/// Ends an agent whose turn is over (ADR 0048 decision 7): the wrapper closes its stdin, waits
/// [`AGENT_EXIT_WAIT`] for it to exit on its own, and stops the process group if it is still
/// there. Closing the stream first is what `radian acp` shuts down on, so a completed attempt
/// records its own exit code (`0`) instead of the SIGTERM one.
fn end_agent(child: &mut Child, client: &acp::Client, grace: Duration) -> i32 {
    client.close_input();
    let deadline = Instant::now() + AGENT_EXIT_WAIT;
    loop {
        match exited(child) {
            Ok(Some(status)) => {
                // Nothing the agent started outlives the attempt, as after any other exit.
                stop_group(child.id(), grace);
                let _ = child.wait();
                return exit_code_of(&status);
            }
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL);
    }
    stop_agent(child, grace).map_or(-1, |status| exit_code_of(&status))
}

/// The repository's git control files, recorded before a worker starts and compared after it
/// exits (ADR 0048 decisions 7 and 8). A worker that can write these files changes what every
/// later git command runs — a hook, a filter driver or `core.fsmonitor` — so the wrapper
/// restores them and fails the attempt.
struct GitControl {
    /// The common git directory (`<repository>/.git` for a normal clone).
    common: PathBuf,
    /// The attempt worktree's own git directory (a `worktrees/<task>` directory under the
    /// common dir for a linked worktree).
    git_dir: PathBuf,
    /// Every control file that existed before the worker, and its content.
    files: BTreeMap<PathBuf, RecordedFile>,
}

/// What a recorded control file held. Content, not time, is what the check compares (decision
/// 7). A symbolic link is recorded by its target, so a link and a regular file at the same
/// path are a change either way, and restoring recreates what was there. An oversized file is
/// not read whole: it is recorded as itself, so it is a change at a path the record holds and
/// an extra file at one it does not.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedFile {
    Regular(Vec<u8>),
    Symlink(PathBuf),
    Oversized,
}

/// What [`GitControl::check`] found (ADR 0048 decisions 7 and 8).
enum ControlCheck {
    /// Every recorded file is as it was and nothing new exists.
    Unchanged,
    /// A control file was created, removed, changed or could not be read back; the restore was
    /// attempted.
    Changed(ControlChange),
}

/// One check that found a change.
struct ControlChange {
    /// The paths that were created, removed or changed, relative to the common dir when they
    /// lie under it.
    paths: Vec<String>,
    /// Why restoring them failed. The attempt then carries both reasons and runs no further
    /// git command (ADR 0048 decisions 7 and 8).
    restore_failed: Option<String>,
}

impl ControlChange {
    /// The failure text `AttemptFinished` and `show` carry.
    fn reason(&self) -> String {
        let mut reason = if self.paths.is_empty() {
            "the worker changed git control files".to_owned()
        } else {
            format!(
                "the worker changed git control files: {}",
                self.paths.join(", ")
            )
        };
        if let Some(error) = &self.restore_failed {
            reason.push_str(&format!("; restoring them failed: {error}"));
        }
        reason
    }
}

impl GitControl {
    /// Records the control files before the worker starts. A file the wrapper cannot read
    /// fails the attempt before a worker runs: without a record there is nothing to compare
    /// with or restore from.
    fn record(worktree: &Path, clock: &Clock) -> Result<Self, Failure> {
        let git_dir = git_directory(worktree, "--absolute-git-dir", clock)?;
        let common = git_directory(worktree, "--git-common-dir", clock)?;
        let scan = ControlScan::read(&common, &git_dir);
        if let Some(error) = scan.errors.values().next() {
            return Err(Failure::error(format!(
                "cannot record the git control files: {error}"
            )));
        }
        let files = scan.files;
        // A file too large to read whole cannot be compared or restored. Fail before the
        // worker starts rather than run unattended with a control file the check cannot see.
        if let Some(path) = files
            .iter()
            .find_map(|(path, file)| (*file == RecordedFile::Oversized).then_some(path))
        {
            return Err(Failure::error(format!(
                "cannot record the git control files: {} is larger than {CONTROL_FILE_LIMIT} bytes",
                path.display()
            )));
        }
        Ok(Self {
            common,
            git_dir,
            files,
        })
    }

    /// Compares the control files with the record and, on a change, restores them. No git
    /// command of the wrapper runs between the worker exiting and this call. A control file
    /// the scan cannot read back cannot be proven unchanged, so it is a change too: the
    /// restore puts the recorded content back and reports what it could not repair
    /// (ADR 0048 decisions 7 and 8).
    fn check(&self) -> ControlCheck {
        let current = ControlScan::read(&self.common, &self.git_dir);
        let mut paths = BTreeSet::new();
        for (path, before) in &self.files {
            if current.files.get(path) != Some(before) {
                paths.insert(shown_control_path(path, &self.common));
            }
        }
        for path in current.files.keys().chain(current.errors.keys()) {
            if !self.files.contains_key(path) {
                paths.insert(shown_control_path(path, &self.common));
            }
        }
        if paths.is_empty() && current.errors.is_empty() {
            return ControlCheck::Unchanged;
        }
        let restore_failed = self.restore(&current).err();
        ControlCheck::Changed(ControlChange {
            paths: paths.into_iter().collect(),
            restore_failed,
        })
    }

    /// Puts every recorded file back and removes every file that was not there before. It
    /// keeps going after a failure, so one file that cannot be restored does not leave the
    /// others as the worker left them; the errors are reported together.
    fn restore(&self, current: &ControlScan) -> Result<(), String> {
        let mut errors = Vec::new();
        for (path, before) in &self.files {
            if current.files.get(path) == Some(before) {
                continue;
            }
            if let Err(error) = self.restore_one(path, before) {
                errors.push(error);
            }
        }
        for path in current.files.keys().chain(current.errors.keys()) {
            if self.files.contains_key(path) {
                continue;
            }
            // A directory is not a file that runs, and the recorded files under it are already
            // back in place; leave it rather than remove a directory the restore just made.
            // An unreadable directory is the exception: its contents are unknown, so it is
            // removed like an unreadable file.
            let is_dir = fs::symlink_metadata(path)
                .map(|metadata| metadata.is_dir())
                .unwrap_or(false);
            if is_dir && !current.errors.contains_key(path) {
                continue;
            }
            if let Err(error) = remove_control_entry(path) {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn restore_one(&self, path: &Path, before: &RecordedFile) -> Result<(), String> {
        let root = if path.starts_with(&self.common) {
            &self.common
        } else {
            &self.git_dir
        };
        ensure_control_dirs(root, path)?;
        remove_control_entry(path)?;
        match before {
            RecordedFile::Regular(bytes) => fs::write(path, bytes)
                .map_err(|error| format!("cannot write {}: {error}", path.display())),
            RecordedFile::Symlink(target) => write_control_symlink(target, path)
                .map_err(|error| format!("cannot write {}: {error}", path.display())),
            // `record` refuses an oversized file, so a record never carries one.
            RecordedFile::Oversized => Err(format!(
                "{} is larger than {CONTROL_FILE_LIMIT} bytes",
                path.display()
            )),
        }
    }
}

/// Resolves a git directory the wrapper asks `git rev-parse` for, relative to the worktree
/// when git answers with a relative path.
fn git_directory(worktree: &Path, flag: &str, clock: &Clock) -> Result<PathBuf, Failure> {
    let text = git(worktree, &["rev-parse", flag], clock)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(Failure::error(format!(
            "git rev-parse {flag} named no directory"
        )));
    }
    let path = Path::new(text);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        worktree.join(path)
    };
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

/// The largest control file the wrapper reads whole. A git config and a hook are small; a
/// larger file is never loaded into memory, so a worker cannot exhaust the wrapper with one.
const CONTROL_FILE_LIMIT: u64 = 1024 * 1024;

/// The one path under `<common dir>/` that the control record leaves out: `info/refs`. Any
/// automatic `git gc` in the shared repository runs `git repack`, which rewrites this file
/// through `git update-server-info`, and only the dumb HTTP transport reads it; a change to it
/// says nothing about what the wrapper's own git commands run (ADR 0048 decision 7).
const INFO_REFS: &str = "info/refs";

/// Every git control file of decision 7 read in one pass, plus the paths the pass could not
/// read. A scan that cannot read one path keeps the rest: the check treats the unreadable path
/// as a change and restores what it recorded (ADR 0048 decisions 7 and 8).
struct ControlScan {
    files: BTreeMap<PathBuf, RecordedFile>,
    /// The paths that could not be read, with the error, keyed by the path that failed (a file
    /// or a directory the scan could not list).
    errors: BTreeMap<PathBuf, String>,
}

impl ControlScan {
    fn read(common: &Path, git_dir: &Path) -> Self {
        let mut scan = Self {
            files: BTreeMap::new(),
            errors: BTreeMap::new(),
        };
        for path in [
            common.join("config"),
            git_dir.join("config.worktree"),
            common.join("config.worktree"),
        ] {
            scan.entry(&path);
        }
        scan.tree(&common.join("hooks"));
        let info = common.join("info");
        scan.tree_except(&info, Some(&common.join(INFO_REFS)));
        scan.modules(&common.join("modules"), false);
        scan
    }

    fn unreadable(&mut self, path: &Path, error: std::io::Error) {
        self.errors.insert(
            path.to_path_buf(),
            format!("cannot read {}: {error}", path.display()),
        );
    }

    fn entry(&mut self, path: &Path) {
        match recorded_entry(path) {
            Ok(Some(recorded)) => {
                self.files.insert(path.to_path_buf(), recorded);
            }
            Ok(None) => {}
            Err(error) => {
                self.errors.insert(path.to_path_buf(), error);
            }
        }
    }

    /// Every non-directory entry under `dir`, recursively, without following a link. A link or
    /// a file where the directory itself should be is recorded as that entry, so swapping the
    /// directory for a link is a change.
    fn tree(&mut self, dir: &Path) {
        self.tree_except(dir, None);
    }

    /// `tree`, leaving out one exact path. `info/refs` is the only entry read this way
    /// ([`INFO_REFS`]). A directory at that path is still walked: the exclusion names that
    /// one path, not what is below it.
    fn tree_except(&mut self, dir: &Path, skip: Option<&Path>) {
        let metadata = match fs::symlink_metadata(dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.unreadable(dir, error);
                return;
            }
        };
        if !metadata.is_dir() {
            self.entry(dir);
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.unreadable(dir, error);
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.unreadable(dir, error);
                    continue;
                }
            };
            let path = entry.path();
            let skipped = skip == Some(path.as_path());
            match entry.file_type() {
                // A directory at the excluded path is still walked: a worker can put one where
                // `info/refs` was, and the files it adds under it are control files like any
                // other under `info/` ([`INFO_REFS`]).
                Ok(kind) if kind.is_dir() => self.tree_except(&path, skip),
                Ok(_) if skipped => {}
                Ok(_) => self.entry(&path),
                // The excluded path stays out even when its kind cannot be read; it was never
                // an entry of the record.
                Err(_) if skipped => {}
                Err(error) => self.unreadable(&path, error),
            }
        }
    }

    /// `config` files and `hooks/` entries anywhere under the submodule gitdirs
    /// (`<common dir>/modules/**`, nested submodules included).
    fn modules(&mut self, dir: &Path, under_hooks: bool) {
        let metadata = match fs::symlink_metadata(dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.unreadable(dir, error);
                return;
            }
        };
        if !metadata.is_dir() {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                self.unreadable(dir, error);
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.unreadable(dir, error);
                    continue;
                }
            };
            let path = entry.path();
            let name = entry.file_name();
            let is_hooks = name == std::ffi::OsStr::new("hooks");
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => self.modules(&path, under_hooks || is_hooks),
                Ok(_) if under_hooks || name == std::ffi::OsStr::new("config") => self.entry(&path),
                Ok(_) => {}
                Err(error) => self.unreadable(&path, error),
            }
        }
    }
}

/// One entry's recorded content: a regular file's bytes or a symbolic link's target. A
/// directory, a FIFO, a device and a missing path record nothing; creating one where the
/// wrapper recorded a file is still a change.
fn recorded_entry(path: &Path) -> Result<Option<RecordedFile>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        return Ok(Some(RecordedFile::Symlink(target)));
    }
    if !kind.is_file() {
        return Ok(None);
    }
    if metadata.len() > CONTROL_FILE_LIMIT {
        return Ok(Some(RecordedFile::Oversized));
    }
    // One handle for the size check and the read, at most one byte past the limit: a file
    // that grew after the check above is still not read into memory.
    let file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    (&file)
        .take(CONTROL_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() as u64 > CONTROL_FILE_LIMIT {
        return Ok(Some(RecordedFile::Oversized));
    }
    Ok(Some(RecordedFile::Regular(bytes)))
}

/// A changed path as the failure text shows it: relative to the common dir when it lies under
/// it (`config`, `hooks/pre-commit`, `worktrees/<task>/config.worktree`).
fn shown_control_path(path: &Path, common: &Path) -> String {
    path.strip_prefix(common)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Creates the parent directories of a restore under `root`, replacing a symbolic link or a
/// file the worker put where a directory belongs. A restore never follows a link out of the
/// git directory.
fn ensure_control_dirs(root: &Path, path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Ok(relative) = parent.strip_prefix(root) else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                remove_control_entry(&current)?;
                fs::create_dir(&current)
                    .map_err(|error| format!("cannot create {}: {error}", current.display()))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)
                    .map_err(|error| format!("cannot create {}: {error}", current.display()))?;
            }
            Err(error) => {
                return Err(format!("cannot read {}: {error}", current.display()));
            }
        }
    }
    Ok(())
}

/// Removes a file, a symbolic link or a directory tree at `path`. A directory is removed
/// deeply: a worker can put one where a recorded file was.
fn remove_control_entry(path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let result = if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| format!("cannot remove {}: {error}", path.display()))
}

#[cfg(unix)]
fn write_control_symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn write_control_symlink(target: &Path, path: &Path) -> std::io::Result<()> {
    fs::write(path, target.to_string_lossy().as_bytes())
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

/// The longest first line of a `Result.text` the wrapper commits as its subject (ADR 0048
/// decision 8).
const COMMIT_SUBJECT_MAX_CHARS: usize = 72;

/// `flows.<name>.commit_dirty`: commit what a `completed` worker left uncommitted or
/// untracked, after the control-file check and before the delivery check (ADR 0048 decision
/// 8). `git add -A` and `git commit` go through the crate's one git helper, with the identity
/// the `merge-main` merge commit uses (`arc <arc@localhost>`). The attempt's notes gain
/// `committed dirty worktree: <short sha>`.
fn commit_dirty(
    o: &Options,
    s: &Setup,
    result: Option<&Value>,
    notes: &mut Vec<String>,
) -> Result<(), Failure> {
    if dirty_paths(&s.worktree, &s.clock)?.is_empty() {
        return Ok(());
    }
    let subject = commit_subject(&o.task, result);
    git(&s.worktree, &["add", "-A"], &s.clock)?;
    let mut args = crate::git::IDENTITY.to_vec();
    args.extend(["commit", "-m", subject.as_str()]);
    git(&s.worktree, &args, &s.clock)?;
    let sha = git(&s.worktree, &["rev-parse", "HEAD"], &s.clock)?;
    let sha = sha.trim();
    notes.push(format!(
        "committed dirty worktree: {}",
        &sha[..sha.len().min(7)]
    ));
    Ok(())
}

/// The wrapper's commit subject (ADR 0048 decision 8): the first paragraph of the worker's
/// `Result.text`, trimmed, first line only, cut at a word boundary to at most 72 characters;
/// `<task id>: worker changes` when the text is missing or empty.
fn commit_subject(task: &str, result: Option<&Value>) -> String {
    let text = result
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let first = text.trim().lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        return format!("{task}: worker changes");
    }
    if first.chars().count() <= COMMIT_SUBJECT_MAX_CHARS {
        return first.to_owned();
    }
    let cut: String = first.chars().take(COMMIT_SUBJECT_MAX_CHARS).collect();
    // The 73rd character decides whether the cut ends on a word: a space there means the
    // 72-character prefix is the whole word.
    if first.chars().nth(COMMIT_SUBJECT_MAX_CHARS) == Some(' ') {
        return cut.trim_end().to_owned();
    }
    match cut.rfind(' ') {
        Some(index) => cut[..index].trim_end().to_owned(),
        None => cut,
    }
}

fn push(repo: &Path, remote: &str, branch: &str, clock: &Clock) -> Result<(), Failure> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = git_remote_raw(repo, &["push", remote, &refspec], clock)?;
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
    let out = git_remote_raw(repo, &["ls-remote", remote, &reference], clock)?;
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

/// A git command of the wrapper that talks to a remote (`fetch`, `push`, `ls-remote`): the
/// shorter of the attempt's remaining wall time and `[limits] git_remote_timeout_secs` bounds
/// it (ADR 0048 decision 7.8). A limit that runs out kills the command's process group; when
/// the wall clock is the shorter limit it is the attempt's `wall_time` failure, and otherwise
/// the call fails with `git <subcommand> timed out after <n> s`.
fn git_remote_raw(cwd: &Path, args: &[&str], clock: &Clock) -> Result<GitOutput, Failure> {
    if clock.expired() {
        clock.mark()?;
        return Err(Failure::wall());
    }
    let key = crate::git::remote_timeout(cwd);
    let (limit, wall_wins) = remote_limit(key, clock.remaining());
    match crate::git::run_with_timeout(cwd, args, Some(limit)) {
        Ok(crate::git::TimedRun::Done(output)) => Ok(output),
        Ok(crate::git::TimedRun::TimedOut) if wall_wins => {
            clock.mark()?;
            Err(Failure::wall())
        }
        Ok(crate::git::TimedRun::TimedOut) => {
            Err(Failure::error(crate::git::timeout_text(args, limit)))
        }
        Err(error) => Err(Failure::error(error.to_string())),
    }
}

/// The limit of one remote call of the wrapper: the shorter of the attempt's remaining wall
/// time and `[limits] git_remote_timeout_secs` (ADR 0048 decision 7.8). `true` when the wall
/// clock is the shorter one, so the failure is the attempt's `wall_time` and not the call's.
fn remote_limit(key: Duration, remaining: Option<Duration>) -> (Duration, bool) {
    match remaining {
        Some(remaining) => (remaining.min(key), remaining <= key),
        None => (key, false),
    }
}

/// Fetches from a remote under the repository's one fetch lock (ADR 0048 decision 10). The
/// shorter of the attempt's remaining wall time and `[limits] git_remote_timeout_secs` bounds
/// both the wait for the lock and the fetch itself; a wait that runs out fails the step with a
/// line that names the lock, not with the attempt's wall-time failure.
fn fetch_remote(cwd: &Path, args: &[&str], clock: &Clock) -> Result<(), Failure> {
    if clock.expired() {
        clock.mark()?;
        return Err(Failure::wall());
    }
    let key = crate::git::remote_timeout(cwd);
    let (limit, wall_wins) = remote_limit(key, clock.remaining());
    match crate::git::fetch(cwd, args, limit) {
        Ok(crate::git::FetchRun::Done(output)) if output.status.success() => Ok(()),
        Ok(crate::git::FetchRun::Done(output)) => Err(Failure::error(format!(
            "git {} failed: {}",
            args.join(" "),
            output.stderr.trim()
        ))),
        Ok(crate::git::FetchRun::TimedOut { limit: _ }) if wall_wins => {
            clock.mark()?;
            Err(Failure::wall())
        }
        Ok(crate::git::FetchRun::TimedOut { limit }) => {
            Err(Failure::error(crate::git::timeout_text(args, limit)))
        }
        Ok(crate::git::FetchRun::LockTimedOut { limit, lock }) => {
            Err(Failure::error(crate::git::lock_timeout_text(&lock, limit)))
        }
        Err(error) => Err(Failure::error(error.to_string())),
    }
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

fn read_stdin(clock: &Clock) -> Result<String, Failure> {
    if clock.expired() {
        clock.mark()?;
        return Err(Failure::wall());
    }
    // A read on stdin that never sees EOF would outlive the attempt's wall time. The reader
    // thread stays blocked if the deadline passes, which does not keep the process alive; the
    // caller gets the wall-time failure and writes the attempt's records.
    let Some(remaining) = clock.remaining() else {
        return read_stdin_blocking();
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read_stdin_blocking().map_err(|failure| failure.message));
    });
    match receiver.recv_timeout(remaining) {
        Ok(Ok(brief)) => Ok(brief),
        Ok(Err(message)) => Err(Failure::error(message)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            clock.mark()?;
            Err(Failure::wall())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Failure::error(
            "cannot read the brief from stdin: the reader ended",
        )),
    }
}

fn read_stdin_blocking() -> Result<String, Failure> {
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

    /// ADR 0048 decision 7: only a `resume`-mode attempt that finds its session continues it.
    /// A session file alone never resumes (that is the mutation "pass `--resume` with
    /// `fresh`"), and a fresh attempt never does.
    #[test]
    fn a_session_starts_new_and_only_a_resume_attempt_with_a_file_continues() {
        assert_eq!(session_start(false, false), SessionStart::New);
        assert_eq!(
            session_start(false, true),
            SessionStart::New,
            "a fresh attempt never resumes, even when a file has that name"
        );
        assert_eq!(
            session_start(true, false),
            SessionStart::New,
            "an earlier attempt that never reached the model left nothing to resume"
        );
        assert_eq!(session_start(true, true), SessionStart::Continue);
    }

    /// The wrapper's commit subject is the first paragraph's first line of `Result.text`,
    /// trimmed, cut at a word boundary to 72 characters, else `<task>: worker changes`
    /// (ADR 0048 decision 8).
    #[test]
    fn the_commit_subject_is_the_first_line_of_the_result_text_or_the_task() {
        let text = |value: &str| json!({"type": "Result", "outcome": "completed", "text": value});
        assert_eq!(
            commit_subject("TK0001-a", Some(&text("  Add the image\n\nmore  "))),
            "Add the image"
        );
        assert_eq!(
            commit_subject("TK0001-a", Some(&text("   \n\nsecond"))),
            "second"
        );
        assert_eq!(
            commit_subject("TK0001-a", Some(&text(""))),
            "TK0001-a: worker changes"
        );
        assert_eq!(commit_subject("TK0001-a", None), "TK0001-a: worker changes");
        assert_eq!(
            commit_subject(
                "TK0001-a",
                Some(&json!({"type": "Result", "outcome": "completed"}))
            ),
            "TK0001-a: worker changes",
            "a Result without text uses the task line"
        );

        let long = "Add the image asset to the gallery and update every reference that names it";
        let subject = commit_subject("TK0001-a", Some(&text(long)));
        assert!(subject.chars().count() <= 72, "{subject}");
        assert_eq!(
            subject,
            "Add the image asset to the gallery and update every reference that names"
        );
        assert!(
            !subject.ends_with(' '),
            "the cut leaves no trailing space: {subject:?}"
        );
        let unbroken = "x".repeat(100);
        assert_eq!(
            commit_subject("TK0001-a", Some(&text(&unbroken))),
            "x".repeat(72),
            "a line with no space is cut at the limit"
        );
    }

    /// The ACP `stopReason` mapping (ADR 0048 decisions 7 and 9): only `end_turn` passes, every
    /// protocol reason keeps its own name as a failed outcome, and an unknown reason is
    /// `failed`. Mutation: map `end_turn` to `failed` → this fails.
    #[test]
    fn the_acp_stop_reasons_map_to_outcomes() {
        for (reason, outcome) in [
            ("end_turn", "completed"),
            ("max_tokens", "max_tokens"),
            ("max_turn_requests", "max_turn_requests"),
            ("refusal", "refusal"),
            ("cancelled", "cancelled"),
            ("something_new", "failed"),
        ] {
            assert_eq!(acp_outcome(reason), outcome, "{reason}");
        }
    }

    /// The `radian` worker's `Result` reads `_meta.radian`: `text` is the last assistant
    /// message in full, `cost` is the object `radian exec` wrote (null included), and a missing
    /// `radian` keeps the protocol's own text. Mutation: drop the `cost` line and the attempt's
    /// cost becomes unknown.
    #[test]
    fn the_radian_meta_is_the_results_text_and_cost() {
        let mut result = json!({
            "type": "Result",
            "outcome": "completed",
            "text": "the streamed run",
        });
        apply_radian_meta(
            &mut result,
            &json!({"radian": {
                "cost": {"micros": 12_345, "incomplete": false, "estimated": false},
                "text": "the last message in full",
            }}),
        );
        assert_eq!(result["text"], json!("the last message in full"));
        assert_eq!(
            result["cost"],
            json!({"micros": 12_345, "incomplete": false, "estimated": false})
        );

        // `radian acp` writes `cost: null` when no call had a price: a reported zero, not an
        // unknown cost (ADR 0048 decision 12).
        let mut unpriced = json!({"type": "Result", "outcome": "completed", "text": "x"});
        apply_radian_meta(
            &mut unpriced,
            &json!({"radian": {"cost": null, "text": "x"}}),
        );
        assert_eq!(unpriced["cost"], Value::Null);
        assert!(unpriced.get("cost").is_some());

        let mut plain = json!({"type": "Result", "outcome": "completed", "text": "x"});
        apply_radian_meta(&mut plain, &Value::Null);
        assert_eq!(plain["text"], json!("x"));
        assert!(
            plain.get("cost").is_none(),
            "a flow's ACP agent reports no cost"
        );
    }

    /// The brief's sections keep their order, the footer last, and a missing one takes no
    /// blank line.
    #[test]
    fn the_brief_keeps_the_conflict_hand_off_body_and_footer_order() {
        assert_eq!(compose_brief(&[None, None, Some("body"), None]), "body");
        assert_eq!(compose_brief(&[Some("a"), None, Some("b"), None]), "a\n\nb");
        assert_eq!(
            compose_brief(&[
                Some("conflict"),
                Some("hand-off"),
                Some("body"),
                Some("footer")
            ]),
            "conflict\n\nhand-off\n\nbody\n\nfooter"
        );
        assert_eq!(compose_brief(&[None, None, Some("  "), None]), "");
    }

    /// A path under `runs/` is copied under its own task and attempt ids and the text names
    /// the copy; a path that walks out of `runs/` is left as it is, with one note (ADR 0048
    /// decision 7).
    #[test]
    fn a_verdict_copy_uses_the_paths_task_and_attempt() {
        let dir = crate::test_support::TempDir::named("radian-arc-verdict-copy");
        let arc = dir.join("arc");
        let worktree = dir.join("worktree");
        fs::create_dir_all(&worktree).expect("the worktree");
        let verdict = arc.join("runs/TK0002-b/attempts/TK0002-b-3/verdicts/review.json");
        fs::create_dir_all(verdict.parent().expect("the verdict directory"))
            .expect("the verdict directory");
        fs::write(&verdict, b"the verdict").expect("the verdict");
        let brief = format!("before `{}` after", verdict.display());
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        assert_eq!(
            text,
            "before `.radian/arc-verdicts/TK0002-b/TK0002-b-3/review.json` after"
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(
            fs::read(worktree.join(".radian/arc-verdicts/TK0002-b/TK0002-b-3/review.json"))
                .expect("the copy")
                .as_slice(),
            b"the verdict"
        );

        let outside = arc.join("runs/TK0002-b/attempts/TK0002-b-3/../../../../outside.txt");
        let brief = format!("`{}`", outside.display());
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        assert_eq!(text, brief, "a path out of runs/ is left as it is");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("verdict copy skipped: "), "{notes:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A named verdict that is a symbolic link is not the regular file the brief promises: it
    /// stays in the text with a note (ADR 0048 decision 7).
    #[cfg(unix)]
    #[test]
    fn a_verdict_copy_that_is_a_symlink_is_skipped() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_support::TempDir::named("radian-arc-verdict-source-link");
        let arc = dir.join("arc");
        let worktree = dir.join("worktree");
        fs::create_dir_all(&worktree).expect("the worktree");
        let real = dir.join("real-verdict.json");
        fs::write(&real, b"the verdict").expect("the real verdict");
        let verdict = arc.join("runs/TK0003-c/attempts/TK0003-c-1/verdicts/review.json");
        fs::create_dir_all(verdict.parent().expect("the verdict directory"))
            .expect("the verdict directory");
        symlink(&real, &verdict).expect("the link");
        let brief = format!("Read it: `{}`", verdict.display());
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        assert_eq!(text, brief, "the linked path stays in the text");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("symbolic link"), "{notes:?}");
        assert!(
            !worktree.join(VERDICT_COPY_DIR).exists(),
            "nothing was copied"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A link the worker planted in its worktree does not redirect the copy: neither an
    /// ancestor of the copy nor the copy file itself is followed, and nothing outside the
    /// worktree is written (ADR 0048 decision 7).
    #[cfg(unix)]
    #[test]
    fn a_verdict_copy_never_follows_a_link_in_the_worktree() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_support::TempDir::named("radian-arc-verdict-destination-link");
        let arc = dir.join("arc");
        let worktree = dir.join("worktree");
        let outside = dir.join("outside");
        fs::create_dir_all(&worktree).expect("the worktree");
        fs::create_dir_all(&outside).expect("the outside directory");
        let verdict = arc.join("runs/TK0004-d/attempts/TK0004-d-1/verdicts/review.json");
        fs::create_dir_all(verdict.parent().expect("the verdict directory"))
            .expect("the verdict directory");
        fs::write(&verdict, b"the verdict").expect("the verdict");
        let brief = format!("Read it: `{}`", verdict.display());

        // `.radian/` itself is a link out of the worktree.
        symlink(&outside, worktree.join(".radian")).expect("the ancestor link");
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        assert_eq!(text, brief, "the path stays in the text");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("symbolic link"), "{notes:?}");
        assert!(
            !outside.join("arc-verdicts").exists(),
            "nothing was written through the link"
        );

        // The copy file itself is a link to a file outside the worktree.
        fs::remove_file(worktree.join(".radian")).expect("remove the link");
        let copies = worktree.join(VERDICT_COPY_DIR).join("TK0004-d/TK0004-d-1");
        fs::create_dir_all(&copies).expect("the copy directory");
        let target = outside.join("target.json");
        fs::write(&target, b"keep").expect("the target");
        symlink(&target, copies.join("review.json")).expect("the file link");
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        assert_eq!(text, brief, "the path stays in the text");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("symbolic link"), "{notes:?}");
        assert_eq!(
            fs::read(&target).expect("the target"),
            b"keep",
            "the link's target is untouched"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A directory the wrapper has just created can be renamed and replaced by a link before it
    /// is opened; the copy descends by handle, so the link is refused and nothing outside the
    /// worktree is written (ADR 0048 decision 7).
    #[cfg(unix)]
    #[test]
    fn a_verdict_copy_survives_a_directory_swapped_for_a_link() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_support::TempDir::named("radian-arc-verdict-dir-swap");
        let arc = dir.join("arc");
        let worktree = dir.join("worktree");
        let outside = dir.join("outside");
        fs::create_dir_all(&worktree).expect("the worktree");
        fs::create_dir_all(&outside).expect("the outside directory");
        let verdict = arc.join("runs/TK0005-e/attempts/TK0005-e-1/verdicts/review.json");
        fs::create_dir_all(verdict.parent().expect("the verdict directory"))
            .expect("the verdict directory");
        fs::write(&verdict, b"the verdict").expect("the verdict");
        let brief = format!("Read it: `{}`", verdict.display());

        // At the moment the wrapper has created `.radian`, a process a worker left behind in
        // an earlier attempt moves it away and links the name to a directory outside the
        // worktree: the window a path-based traversal leaves open between check and use.
        let created = worktree.join(".radian");
        let moved = worktree.join(".radian-moved");
        let target = outside.clone();
        AFTER_MKDIR.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |path: &Path| {
                assert_eq!(path, created.as_path());
                fs::rename(path, &moved).expect("the rename");
                symlink(&target, path).expect("the link");
            }));
        });
        let (text, notes) = copy_verdicts(&brief, &arc, &worktree);
        AFTER_MKDIR.with(|hook| hook.borrow_mut().take());
        assert_eq!(text, brief, "the path stays in the text");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("symbolic link"), "{notes:?}");
        assert!(
            !outside.join("arc-verdicts").exists(),
            "nothing was written through the link"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file over the limit, a file that is not UTF-8, and a missing file: the first two get
    /// the one note line and no section; the third is silence (ADR 0048 decision 7).
    #[test]
    fn a_worker_brief_over_the_limit_or_not_utf8_is_skipped_with_a_note() {
        let dir = crate::test_support::TempDir::named("radian-arc-brief");
        let path = dir.join(crate::store::WORKER_BRIEF_FILE);
        assert_eq!(worker_brief(&dir), (None, None), "a missing file is silent");
        fs::write(&path, "  \n\t\n").expect("whitespace");
        assert_eq!(worker_brief(&dir), (None, None), "an empty one too");
        fs::write(&path, "stand here\n").expect("a footer");
        assert_eq!(worker_brief(&dir), (Some("stand here\n".to_owned()), None));
        fs::write(&path, "x".repeat(33 * 1024)).expect("too large");
        let (section, note) = worker_brief(&dir);
        assert!(section.is_none(), "a file over the limit is not appended");
        assert!(
            note.expect("a note")
                .starts_with("worker-brief.md skipped: "),
            "the note names the file"
        );
        fs::write(&path, [0xff, 0xfe, 0xfd]).expect("not UTF-8");
        let (section, note) = worker_brief(&dir);
        assert!(
            section.is_none(),
            "a file that is not UTF-8 is not appended"
        );
        assert_eq!(note.as_deref(), Some("worker-brief.md skipped: not UTF-8"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A FIFO at the path would block a plain open until a writer appears, and the wrapper
    /// composes the brief before any child of its own exists, so no attempt limit can end the
    /// wait: the handle is opened non-blocking, its type is checked, and the file is skipped
    /// with a note (ADR 0048 decision 7).
    #[cfg(unix)]
    #[test]
    fn a_worker_brief_that_is_not_a_regular_file_is_skipped_with_a_note() {
        use std::os::unix::ffi::OsStrExt as _;
        let dir = crate::test_support::TempDir::named("radian-arc-brief-fifo");
        let path = dir.join(crate::store::WORKER_BRIEF_FILE);
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in a path");
        // SAFETY: `name` is a NUL-terminated path this test owns and `mkfifo` only creates it.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0, "mkfifo");
        let (section, note) = worker_brief(&dir);
        assert!(section.is_none(), "a FIFO is not appended");
        let note = note.expect("a note");
        assert!(
            note.starts_with("worker-brief.md skipped: ")
                && note.ends_with("is not a regular file"),
            "{note}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The cap is 8,000 characters, and a cut report says so. A report at the cap is unchanged.
    #[test]
    fn a_long_report_is_cut_at_the_cap_with_a_note() {
        assert_eq!(HANDOFF_REPORT_CHARS, 8_000);
        let at_cap = "x".repeat(HANDOFF_REPORT_CHARS);
        assert_eq!(cap_report(&at_cap), at_cap, "a report at the cap is whole");
        let over = "x".repeat(HANDOFF_REPORT_CHARS + 1);
        let cut = cap_report(&over);
        assert!(cut.starts_with(&at_cap), "the first 8,000 characters stay");
        assert!(
            !cut.contains(&"x".repeat(HANDOFF_REPORT_CHARS + 1)),
            "the rest is cut"
        );
        assert!(
            cut.contains("cut at 8,000 characters"),
            "the note says so: {cut}"
        );
        assert_eq!(
            cap_report("héllo"),
            "héllo",
            "the cap counts characters, not bytes"
        );
    }

    /// Decision 7.2: a later attempt resumes only a session that exists. A first attempt
    /// that failed on a bad worker argument created no session file, and resuming it would
    /// fail every remaining attempt with `no session`.
    #[test]
    fn a_session_exists_only_when_its_file_is_in_the_worker_session_dir() {
        let dir = crate::test_support::TempDir::named("radian-arc-session");
        fs::create_dir_all(&*dir).expect("session dir");
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

    /// The wrapper bounds a remote call by the shorter of the attempt's remaining wall time
    /// and the key, and a wall win is the attempt's `wall_time`, not the call's own failure.
    /// Mutation: always take the key and the attempt's wall-clock failure never fires.
    #[test]
    fn a_remote_call_takes_the_shorter_of_the_wall_clock_and_the_key() {
        let key = Duration::from_secs(300);
        assert_eq!(
            remote_limit(key, Some(Duration::from_secs(5))),
            (Duration::from_secs(5), true)
        );
        assert_eq!(
            remote_limit(key, Some(Duration::from_secs(900))),
            (key, false)
        );
        assert_eq!(remote_limit(key, None), (key, false));
        assert_eq!(
            remote_limit(key, Some(key)),
            (key, true),
            "the wall clock wins a tie"
        );
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
