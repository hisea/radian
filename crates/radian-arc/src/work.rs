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
//!    never run together);
//! 2. write the host-local attempt record under `<repository>/.radian/arc/attempts/`;
//! 3. run `radian exec -p - --session-id <task> --workspace <worktree> --profile workspace
//!    --yes --output jsonl` (with `--resume <task>` for a rework attempt and the caller's
//!    extra arguments), the brief on its stdin, stdout into `runs/…/result.jsonl`;
//! 4. after the child exits, read the `Result` line and the exit code, read `head_sha`,
//!    list uncommitted and untracked files, and run the delivery checks;
//! 5. push `work/<task>` only when the outcome is `completed` and every check passed, then
//!    verify the remote ref with `ls-remote`;
//! 6. print one `AttemptFinished` JSON line as its last stdout line and exit with the
//!    child's exit code (or 1 when a check failed after a `completed` child).
//!
//! Every git command this module runs carries `-c core.hooksPath=` (empty) and
//! `GIT_CONFIG_NOSYSTEM=1`, so no file the model can write changes what the wrapper runs
//! (ADR 0048 decision 7.3). The push credential stays in this process's environment; in v0
//! the wrapper and the model are the same OS user, so that is not isolation.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use fs2::FileExt;
use serde_json::{Value, json};

/// The `radian arc work` flags. `radian-tui`'s `arc work` subcommand fills this.
pub struct Options {
    /// The task id: the session id, the branch suffix and the worktree name.
    pub task: String,
    /// The attempt number, 1-based.
    pub attempt: u32,
    /// The random token the scheduling loop expects back in `AttemptFinished`.
    pub token: String,
    /// Continue the task's existing session (`--resume <task>`) instead of starting one.
    pub resume: bool,
    /// Let a branch with no change past `base_sha` pass the delivery check (ADR 0048
    /// decision 8: a brief can allow a no-op).
    pub allow_noop: bool,
    /// Extra `radian exec` arguments after `--` (the model route in tests: `--model`,
    /// `--base-url`).
    pub extra_args: Vec<String>,
}

/// Exit codes (ADR 0048 decision 1): 0 success, 1 error, 2 usage, 4 a lock is held.
const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_LOCKED: i32 = 4;

/// The arc directory's entries the worker must not read (ADR 0048 decision 5): every entry
/// except `worktrees/`, because the worker's own workspace lives there.
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
    let repo = repo_root()?;
    let arc = repo.join(".radian").join("arc");
    let worktree = arc.join("worktrees").join(&o.task);
    let branch = format!("work/{}", o.task);
    let created = ensure_worktree(&repo, &worktree, &branch)?;
    let base_sha = base_sha(&repo, &arc, o, &branch, created)?;
    // Held until this function returns: one attempt of one task at a time.
    let _lock = lock_worktree(&worktree)?;

    let runs = arc
        .join("runs")
        .join(&o.task)
        .join("attempts")
        .join(o.attempt.to_string());
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
    let started_at = now();
    let deny = deny_paths(&arc);
    let mut child = spawn_child(o, &worktree, &deny, &result_path, &stderr_path, &brief)?;
    let pid = child.id();
    let process_started_at = process_start(pid).unwrap_or_else(|| started_at.clone());
    write_json(
        &attempt_dir.join("identity.json"),
        &json!({
            "attempt_id": format!("{}-{}", o.task, o.attempt),
            "token": o.token,
            "pid": pid,
            "process_started_at": process_started_at,
            "session_id": o.task,
            "worktree": worktree.display().to_string(),
            "branch": branch,
            "started_at": started_at,
        }),
    )?;
    let status = child
        .wait()
        .map_err(|error| Failure::error(format!("cannot wait for the worker: {error}")))?;
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
            "process_started_at": process_started_at,
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
        let check = check_delivery(
            &repo,
            &worktree,
            &branch,
            &base_sha,
            head_sha.as_deref(),
            &status_paths,
            o.allow_noop,
        );
        match check {
            Err(reason) => failure = Some(reason),
            Ok(()) => {
                let head = head_sha.as_deref().unwrap_or_default();
                match push(&repo, &branch) {
                    Err(reason) => failure = Some(reason),
                    Ok(()) => {
                        pushed = true;
                        match remote_sha_of(&repo, &branch) {
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

/// Creates the worktree from `origin/main` when it is missing, or reuses it. Returns whether
/// this run created it (the first attempt records `base_sha` from `origin/main`).
fn ensure_worktree(repo: &Path, worktree: &Path, branch: &str) -> Result<bool, Failure> {
    if let Some(parent) = worktree.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            Failure::error(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
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
        git(repo, &["rev-parse", "--verify", "--quiet", "origin/main"]).map_err(|_| {
            Failure::error("origin/main does not exist; the first attempt starts from origin/main")
        })?;
        git(
            repo,
            &["worktree", "add", "-b", branch, &path, "origin/main"],
        )?;
    }
    Ok(true)
}

/// The task's base commit: `origin/main` when this run created the worktree, else the value
/// the first attempt recorded. A worktree made outside the wrapper falls back to the
/// merge-base with `origin/main`.
fn base_sha(
    repo: &Path,
    arc: &Path,
    o: &Options,
    branch: &str,
    created: bool,
) -> Result<String, Failure> {
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
        git(repo, &["rev-parse", "origin/main"])?
    } else {
        git(repo, &["merge-base", "origin/main", branch])?
    };
    let base = base.trim().to_owned();
    write_text(&path, &base)?;
    Ok(base)
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

fn spawn_child(
    o: &Options,
    worktree: &Path,
    deny: &[PathBuf],
    result_path: &Path,
    stderr_path: &Path,
    brief: &str,
) -> Result<Child, Failure> {
    let executable = std::env::current_exe()
        .map_err(|error| Failure::error(format!("cannot find the current executable: {error}")))?;
    let mut command = Command::new(executable);
    command.arg("exec");
    // `--session-id` and `--resume` are exclusive in `radian exec` (ADR 0022/0023): a
    // rework attempt resumes the task's session, a first attempt names it.
    if o.resume {
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

/// The checks of ADR 0048 decisions 7.4 and 8, in order. The first failure names its
/// reason; nothing is pushed when one fails.
fn check_delivery(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
    head: Option<&str>,
    status_paths: &Result<Vec<String>, String>,
    allow_noop: bool,
) -> Result<(), String> {
    let branch_ref = format!("refs/heads/{branch}");
    let exists = git_raw(repo, &["rev-parse", "--verify", "--quiet", &branch_ref])
        .map_err(|error| error.message)?;
    if !exists.status.success() {
        return Err(format!("branch {branch} does not exist"));
    }
    let head = head.ok_or_else(|| "cannot read the worktree HEAD".to_owned())?;
    let ancestor = git_raw(repo, &["merge-base", "--is-ancestor", base, head])
        .map_err(|error| error.message)?;
    if !ancestor.status.success() {
        return Err(format!("head {head} does not descend from base {base}"));
    }
    let head_ref =
        git_raw(worktree, &["symbolic-ref", "--short", "HEAD"]).map_err(|error| error.message)?;
    if !head_ref.status.success() || head_ref.stdout.trim() != branch {
        return Err(format!("the worktree HEAD is not on {branch}"));
    }
    let dirty = match status_paths {
        Ok(paths) => paths,
        Err(reason) => return Err(reason.clone()),
    };
    if !dirty.is_empty() {
        return Err(format!(
            "uncommitted or untracked files: {}",
            dirty.join(", ")
        ));
    }
    let changed = changed_paths(repo, base, head)?;
    if changed.is_empty() && !allow_noop {
        return Err(format!("no change: {head} adds nothing to {base}"));
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

fn push(repo: &Path, branch: &str) -> Result<(), String> {
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    let out = git_raw(repo, &["push", "origin", &refspec]).map_err(|error| error.message)?;
    if !out.status.success() {
        return Err(format!("push failed: {}", out.stderr.trim()));
    }
    Ok(())
}

fn remote_sha_of(repo: &Path, branch: &str) -> Result<String, String> {
    let reference = format!("refs/heads/{branch}");
    let out = git_raw(repo, &["ls-remote", "origin", &reference]).map_err(|error| error.message)?;
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

/// A git command of the wrapper (ADR 0048 decision 7.3): hooks off, no system config, and
/// no interactive credential prompt (an unattended wrapper must never wait for one).
fn git_command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .arg("-c")
        .arg("core.hooksPath=")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

struct GitOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn git_raw(cwd: &Path, args: &[&str]) -> Result<GitOutput, Failure> {
    let output = git_command(cwd)
        .args(args)
        .output()
        .map_err(|error| Failure::error(format!("cannot run git: {error}")))?;
    Ok(GitOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
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
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
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
