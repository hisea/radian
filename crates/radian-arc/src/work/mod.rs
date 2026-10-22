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
//! 4. after the child exits, read the `Result` line and the exit code, read `head_sha`,
//!    list uncommitted and untracked files, and run the delivery checks;
//! 5. push `work/<task>` only when the outcome is `completed` and every check passed, then
//!    verify the remote ref with `ls-remote`;
//! 6. print one `AttemptFinished` JSON line as its last stdout line and exit with the
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
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use fs2::FileExt;
use serde_json::{Value, json};

use crate::git::GitOutput;
use crate::process;

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
static TERM_REQUESTED: AtomicBool = AtomicBool::new(false);

/// The SIGTERM handler: remember the request and forward it to the worker's whole process
/// group. Both the atomic stores and `killpg(2)` are async-signal-safe.
extern "C" fn forward_sigterm(_signal: libc::c_int) {
    TERM_REQUESTED.store(true, Ordering::SeqCst);
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        process::signal_group(pid as u32, process::SIGTERM);
    }
}

fn install_sigterm_handler() {
    // SAFETY: the handler only touches atomics and calls `kill(2)`; both are
    // async-signal-safe. `signal` with a positive handler installs it for the process.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            forward_sigterm as *const () as libc::sighandler_t,
        );
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
}

impl Failure {
    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_ERROR,
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: EXIT_USAGE,
        }
    }
}

fn run_inner(o: &Options) -> Result<i32, Failure> {
    validate(o)?;
    // ADR 0048 decision 9: arc cancels an attempt with SIGTERM and expects the wrapper to
    // pass it on, wait for the worker, and still write `AttemptFinished`. Without a handler
    // the wrapper would die at once and leave no record for arc to apply.
    install_sigterm_handler();
    let repo = repo_root()?;
    let arc = repo.join(".radian").join("arc");
    let worktree = arc.join("worktrees").join(&o.task);
    let branch = format!("work/{}", o.task);
    let created = ensure_worktree(&repo, &worktree, &branch, &o.remote, &o.main_branch)?;
    // Held until this function returns: one attempt of one task at a time.
    let _lock = lock_worktree(&worktree)?;

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
    let result_path = runs.join("result.jsonl");
    let stderr_path = runs.join("stderr.log");
    let brief = read_stdin()?;
    let base_sha = base_sha(&repo, &arc, o, &branch, created)?;
    // The moved-ref guard: a later attempt may only run when `work/<task>` still points at
    // the tip the previous attempt ended on. Another worker can move the ref from inside its
    // `workspace` sandbox (the common git directory's refs are writable, ADR 0038), and with
    // a matching tree every delivery check would pass; so this check runs before the child,
    // and nothing is recorded for the next attempt when it fails.
    if let Some(last) = last_head(&arc, &o.task)
        && let Err(reason) = check_start(&repo, &worktree, &branch, &last)
    {
        return refuse(o, &runs, &attempt_dir, &base_sha, &reason);
    }
    let start_sha = branch_tip(&repo, &branch)
        .ok_or_else(|| Failure::error(format!("cannot read {branch} in {}", repo.display())))?;
    let started_at = now();
    let deny = deny_paths(&arc);
    let mut child = spawn_child(o, &worktree, &deny, &result_path, &stderr_path, &brief)?;
    let pid = child.id();
    CHILD_PID.store(pid as i32, Ordering::SeqCst);
    // A SIGTERM that arrived during the worktree setup is forwarded as soon as there is a
    // child to forward it to.
    if TERM_REQUESTED.load(Ordering::SeqCst) {
        process::signal_group(pid, process::SIGTERM);
    }
    let processor_started_at = process_start(pid).unwrap_or_else(|| started_at.clone());
    let wrapper_pid = std::process::id();
    write_json(
        &attempt_dir.join("identity.json"),
        &json!({
            "attempt_id": format!("{}-{}", o.task, o.attempt),
            "token": o.token,
            "pid": pid,
            "process_started_at": processor_started_at,
            // The scheduling side signals the wrapper, not the worker, so the attempt
            // record carries the wrapper's own identity too (decision 9).
            "wrapper_pid": wrapper_pid,
            "wrapper_process_started_at": process::start_time(wrapper_pid),
            "session_id": o.task,
            "worktree": worktree.display().to_string(),
            "branch": branch,
            "started_at": started_at,
        }),
    )?;
    let status = child
        .wait()
        .map_err(|error| Failure::error(format!("cannot wait for the worker: {error}")))?;
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
    let head_sha = git(&worktree, &["rev-parse", "HEAD"])
        .ok()
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| !sha.is_empty());
    let status_paths = dirty_paths(&worktree);
    let dirty = status_paths.clone().unwrap_or_default();
    write_json(
        &runs.join("execution.json"),
        &json!({
            "attempt_id": format!("{}-{}", o.task, o.attempt),
            "host": hostname(),
            "token": o.token,
            "pid": pid,
            "process_started_at": processor_started_at,
            "wrapper_pid": wrapper_pid,
            "session_id": o.task,
            "worktree": worktree.display().to_string(),
            "branch": branch,
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
        let check = check_delivery(&Delivery {
            repo: &repo,
            worktree: &worktree,
            branch: &branch,
            base: &base_sha,
            start: &start_sha,
            head: head_sha.as_deref(),
            status_paths: &status_paths,
            allow_noop: o.allow_noop,
        });
        match check {
            Err(reason) => failure = Some(reason),
            Ok(()) => {
                let head = head_sha.as_deref().unwrap_or_default();
                match push(&repo, &o.remote, &branch) {
                    Err(reason) => failure = Some(reason),
                    Ok(()) => {
                        pushed = true;
                        match remote_sha_of(&repo, &o.remote, &branch) {
                            Err(reason) => failure = Some(reason),
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

    let attempt_failed = failure.is_some();
    // Every attempt that ran records the branch tip it ended on (host-local): a later attempt
    // must still find that tip on `work/<task>` before it starts (the guard above). A tip
    // that cannot be read (the child removed the branch) leaves the previous record in
    // place, so the next attempt refuses instead of running on a branch it cannot trust.
    if let Some(tip) = branch_tip(&repo, &branch) {
        write_text(&last_head_path(&arc, &o.task), &tip)?;
    }
    let record = json!({
        "type": "AttemptFinished",
        "attempt_id": format!("{}-{}", o.task, o.attempt),
        "token": o.token,
        "exit_code": exit_code,
        "outcome": outcome,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "dirty": dirty,
        "pushed": pushed,
        "remote_sha": remote_sha,
        "failure": failure,
    });
    let line = serde_json::to_string(&record)
        .map_err(|error| Failure::error(format!("cannot serialize AttemptFinished: {error}")))?;
    write_text(&runs.join("attempt_finished.json"), &line)?;
    write_text(&attempt_dir.join("status.json"), &line)?;
    println!("{line}");

    // ADR 0044 section 3: the child's exit code passes through. A check that failed after a
    // `completed` (exit 0) child is this wrapper's own failure: exit 1.
    Ok(if attempt_failed && exit_code == 0 {
        EXIT_ERROR
    } else {
        exit_code
    })
}

/// The task id is a file name and a branch suffix: keep it to characters that are safe in
/// both, so a caller cannot walk out of the arc directory or name an option.
fn validate(o: &Options) -> Result<(), Failure> {
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
    let top = git(&cwd, &["rev-parse", "--show-toplevel"])
        .map_err(|_| Failure::error(format!("{} is not inside a git repository", cwd.display())))?;
    let root = PathBuf::from(top.trim());
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
        let top = git(worktree, &["rev-parse", "--show-toplevel"]).map_err(|error| {
            Failure::error(format!(
                "{} exists but is not a usable worktree: {}",
                worktree.display(),
                error.message
            ))
        })?;
        if !same_path(Path::new(top.trim()), worktree) {
            return Err(Failure::error(format!(
                "{} is not the worktree of {}",
                worktree.display(),
                repo.display()
            )));
        }
        let common = git(worktree, &["rev-parse", "--git-common-dir"]).map_err(|error| {
            Failure::error(format!(
                "cannot read the worktree's common git directory: {}",
                error.message
            ))
        })?;
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
    let branch_exists = git_raw(repo, &["rev-parse", "--verify", "--quiet", &branch_ref])?
        .status
        .success();
    if branch_exists {
        git(repo, &["worktree", "add", &path, branch])?;
    } else {
        // The first attempt starts from the current remote main, not from whatever this
        // clone last fetched. The refspec names the tracking ref explicitly so a remote
        // with a different fetch configuration cannot leave it stale.
        let start = format!("{remote}/{main_branch}");
        let refspec = format!("+refs/heads/{main_branch}:refs/remotes/{start}");
        git(repo, &["fetch", remote, &refspec]).map_err(|error| {
            Failure::error(format!(
                "cannot fetch {remote} {main_branch}: {}",
                error.message
            ))
        })?;
        if !git_raw(repo, &["rev-parse", "--verify", "--quiet", &start])?
            .status
            .success()
        {
            return Err(Failure::error(format!(
                "{start} does not exist; a first attempt starts from the remote's main branch"
            )));
        }
        git(repo, &["worktree", "add", "-b", branch, &path, &start])?;
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
        git(repo, &["rev-parse", &main])?
    } else {
        git(repo, &["merge-base", &main, branch])?
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
/// run.
fn check_start(repo: &Path, worktree: &Path, branch: &str, last: &str) -> Result<(), String> {
    let reference = format!("refs/heads/{branch}");
    let tip = git_raw(repo, &["rev-parse", "--verify", "--quiet", &reference])
        .map_err(|error| error.message)?;
    if !tip.status.success() || tip.stdout.trim() != last {
        return Err(moved_outside(branch));
    }
    let head =
        git_raw(worktree, &["symbolic-ref", "--short", "HEAD"]).map_err(|error| error.message)?;
    if !head.status.success() || head.stdout.trim() != branch {
        return Err(moved_outside(branch));
    }
    Ok(())
}

fn moved_outside(branch: &str) -> String {
    format!("{branch} moved outside this task")
}

/// A start refusal: no child runs and nothing is pushed, but the attempt still ends with its
/// `AttemptFinished` record so the scheduling loop reads the reason. The record is not
/// written as `last_head`: the moved tip is exactly what a later attempt must keep refusing.
fn refuse(
    o: &Options,
    runs: &Path,
    attempt_dir: &Path,
    base_sha: &str,
    reason: &str,
) -> Result<i32, Failure> {
    let record = json!({
        "type": "AttemptFinished",
        "attempt_id": format!("{}-{}", o.task, o.attempt),
        "token": o.token,
        "exit_code": EXIT_ERROR,
        "outcome": "terminated",
        "base_sha": base_sha,
        "head_sha": Value::Null,
        "dirty": Vec::<String>::new(),
        "pushed": false,
        "remote_sha": Value::Null,
        "failure": reason,
    });
    let line = serde_json::to_string(&record)
        .map_err(|error| Failure::error(format!("cannot serialize AttemptFinished: {error}")))?;
    write_text(&runs.join("attempt_finished.json"), &line)?;
    write_text(&attempt_dir.join("status.json"), &line)?;
    println!("{line}");
    Ok(EXIT_ERROR)
}

/// `refs/heads/<branch>` in the main clone, when it exists.
fn branch_tip(repo: &Path, branch: &str) -> Option<String> {
    let reference = format!("refs/heads/{branch}");
    let out = git_raw(repo, &["rev-parse", "--verify", "--quiet", &reference]).ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = out.stdout.trim().to_owned();
    (!sha.is_empty()).then_some(sha)
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
    /// The `work/<task>` tip this attempt started from.
    start: &'a str,
    head: Option<&'a str>,
    status_paths: &'a Result<Vec<String>, String>,
    allow_noop: bool,
}

/// The checks of ADR 0048 decisions 7.4 and 8, in order. The first failure names its
/// reason; nothing is pushed when one fails.
fn check_delivery(d: &Delivery<'_>) -> Result<(), String> {
    let branch = d.branch;
    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_raw(d.repo, &["rev-parse", "--verify", "--quiet", &branch_ref])
        .map_err(|error| error.message)?;
    if !exists.status.success() {
        return Err(format!("branch {branch} does not exist"));
    }
    let head = d
        .head
        .ok_or_else(|| "cannot read the worktree HEAD".to_owned())?;
    let ancestor = git_raw(d.repo, &["merge-base", "--is-ancestor", d.base, head])
        .map_err(|error| error.message)?;
    if !ancestor.status.success() {
        return Err(format!("head {head} does not descend from base {}", d.base));
    }
    // The child may have rewritten its own branch. It must still contain the tip this
    // attempt started from; otherwise the attempt would deliver a rollback.
    let from_start = git_raw(d.repo, &["merge-base", "--is-ancestor", d.start, head])
        .map_err(|error| error.message)?;
    if !from_start.status.success() {
        return Err(format!(
            "head {head} does not descend from the start {}",
            d.start
        ));
    }
    let head_ref =
        git_raw(d.worktree, &["symbolic-ref", "--short", "HEAD"]).map_err(|error| error.message)?;
    if !head_ref.status.success() || head_ref.stdout.trim() != branch {
        return Err(format!("the worktree HEAD is not on {branch}"));
    }
    let dirty = match d.status_paths {
        Ok(paths) => paths,
        Err(reason) => return Err(reason.clone()),
    };
    if !dirty.is_empty() {
        return Err(format!(
            "uncommitted or untracked files: {}",
            dirty.join(", ")
        ));
    }
    let changed = changed_paths(d.repo, d.base, head)?;
    if changed.is_empty() && !d.allow_noop {
        return Err(format!("no change: {head} adds nothing to {}", d.base));
    }
    let arc_paths: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|path| *path == ".radian/arc" || path.starts_with(".radian/arc/"))
        .collect();
    if !arc_paths.is_empty() {
        return Err(format!(
            "the branch changes files under .radian/arc/: {}",
            arc_paths.join(", ")
        ));
    }
    Ok(())
}

fn changed_paths(repo: &Path, base: &str, head: &str) -> Result<Vec<String>, String> {
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
    )
    .map_err(|error| error.message)?;
    if !out.status.success() {
        return Err(format!(
            "cannot list the branch's changes: {}",
            out.stderr.trim()
        ));
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Uncommitted and untracked paths of the worktree, from `git status --porcelain -z`.
fn dirty_paths(worktree: &Path) -> Result<Vec<String>, String> {
    let out = git_raw(
        worktree,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .map_err(|error| error.message)?;
    if !out.status.success() {
        return Err(format!(
            "cannot list the worktree status: {}",
            out.stderr.trim()
        ));
    }
    Ok(out
        .stdout
        .split('\0')
        .filter(|record| record.len() > 3)
        .map(|record| record[3..].to_owned())
        .collect())
}

fn push(repo: &Path, remote: &str, branch: &str) -> Result<(), String> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = git_raw(repo, &["push", remote, &refspec]).map_err(|error| error.message)?;
    if !out.status.success() {
        return Err(format!("push failed: {}", out.stderr.trim()));
    }
    Ok(())
}

fn remote_sha_of(repo: &Path, remote: &str, branch: &str) -> Result<String, String> {
    let reference = format!("refs/heads/{branch}");
    let out = git_raw(repo, &["ls-remote", remote, &reference]).map_err(|error| error.message)?;
    if !out.status.success() {
        return Err(format!("ls-remote failed: {}", out.stderr.trim()));
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
/// configuration is out, and an unattended command never waits on a credential prompt.
fn git_raw(cwd: &Path, args: &[&str]) -> Result<GitOutput, Failure> {
    crate::git::run(cwd, args).map_err(|error| Failure::error(error.to_string()))
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, Failure> {
    let out = git_raw(cwd, args)?;
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
        assert!(!session_exists("0001-task", &args), "no file yet");
        fs::write(dir.join("0001-task.jsonl"), "").expect("write session");
        assert!(session_exists("0001-task", &args));
        assert!(!session_exists("0002-other", &args), "another task's file");
        let equals = strings(&[&format!("--session-dir={path}")]);
        assert!(session_exists("0001-task", &equals));
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
