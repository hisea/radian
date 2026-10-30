//! The one git command builder for arc.
//!
//! Every git call of the store, the gate and the worker wrapper is built here from
//! [`radian_coding::host_git::host_git_command`], the shared host builder (ADR 0039): it
//! carries the `-c` overrides that neutralise the repository's own filters, hooks,
//! fsmonitor and diff drivers, so no file a person or a worker can write changes what arc
//! runs. On top of that this helper guarantees:
//!
//! - `-c core.hooksPath=` after the shared overrides: the hooks path is empty (and wins over
//!   the shared `/dev/null`), so a repository hook never runs for arc (ADR 0048 decisions 7
//!   and 10);
//! - `GIT_CONFIG_NOSYSTEM=1`: the system configuration does not take part;
//! - `GIT_TERMINAL_PROMPT=0`: an unattended fetch or push fails at once instead of waiting on
//!   a credential prompt;
//! - `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` are removed: a caller's injected
//!   configuration could otherwise replace the empty hooks path;
//! - `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`
//!   and `GIT_ALTERNATE_OBJECT_DIRECTORIES` are removed: every call names its repository, so
//!   a caller's environment can never point a command at another repository or object store.
//!
//! Every fetch goes through [`fetch`], which serializes arc's own fetches into one repository
//! with a lock in its git common dir (ADR 0048 decision 10).
//!
//! The repository, global and system configuration files are otherwise unaffected.

use std::fmt;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The environment variables that would point git at another repository or object store.
/// [`crate::gate::run_shell`]'s callers that run a user command in a named repository remove
/// them too, so the command's own git calls name the same repository.
pub(crate) const REPOSITORY_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// The identity of the commits arc itself writes: `arc <arc@localhost>`. The `merge-main`
/// merge commit and a `flows.<name>.commit_dirty` commit both carry it (ADR 0048 decision 8),
/// so the two names live here instead of in each caller.
pub(crate) const IDENTITY: [&str; 4] = ["-c", "user.name=arc", "-c", "user.email=arc@localhost"];

/// The environment channels that inject configuration from the caller.
const INJECTED_CONFIG_ENV: [&str; 2] = ["GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"];

/// A finished git command.
pub(crate) struct GitOutput {
    /// The exit status.
    pub(crate) status: ExitStatus,
    /// Standard output, lossily decoded.
    pub(crate) stdout: String,
    /// Standard error, lossily decoded.
    pub(crate) stderr: String,
}

impl GitOutput {
    /// Whether the command exited zero.
    pub(crate) fn success(&self) -> bool {
        self.status.success()
    }
}

/// Why a git command did not run at all.
#[derive(Debug)]
pub(crate) enum RunError {
    /// The shared builder could not read the effective configuration.
    Build(String),
    /// The process could not be started.
    Spawn(std::io::Error),
    /// The fetch lock could not be resolved, created or taken.
    Lock(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Build(message) => f.write_str(message),
            RunError::Spawn(error) => write!(f, "cannot run git: {error}"),
            RunError::Lock(message) => f.write_str(message),
        }
    }
}

/// The command for one git call in `dir`, with the shared overrides and arc's own
/// environment.
fn command(dir: &Path) -> Result<Command, RunError> {
    let mut command = radian_coding::host_git::host_git_command(dir).map_err(RunError::Build)?;
    command
        .arg("-c")
        .arg("core.hooksPath=")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    for name in INJECTED_CONFIG_ENV.into_iter().chain(REPOSITORY_ENV) {
        command.env_remove(name);
    }
    Ok(command)
}

/// Runs a git command in `dir` without checking its exit status.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<GitOutput, RunError> {
    let output = command(dir)?.args(args).output().map_err(RunError::Spawn)?;
    Ok(GitOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// What a git command with a wall-clock limit produced.
pub(crate) enum TimedRun {
    /// The command finished.
    Done(GitOutput),
    /// The limit ran out; the command's whole process group was killed.
    TimedOut,
}

/// Runs a git command in `dir` with an optional wall-clock limit. A limit that runs out kills
/// the command's process group (the command leads its own group), so a fetch or a push that
/// waits on a remote cannot outlive the attempt. `None` waits forever: a local call (a
/// `diff`, a `log`, a `merge-base`, a `status`, a local `merge`) uses [`run`] instead, and
/// every call that talks to a remote uses [`run_remote`] or [`fetch`], both of which always
/// have a limit.
pub(crate) fn run_with_timeout(
    dir: &Path,
    args: &[&str],
    timeout: Option<Duration>,
) -> Result<TimedRun, RunError> {
    let Some(timeout) = timeout else {
        return run(dir, args).map(TimedRun::Done);
    };
    let mut command = command(dir)?;
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Own process group: killing it reaches everything git started (receive-pack, ssh).
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(RunError::Spawn)?;
    #[cfg(unix)]
    let pid = child.id();
    // Drain the pipes while waiting, so a chatty fetch cannot fill them and block.
    let read = |mut pipe: Option<std::process::ChildStdout>| {
        pipe.take().map(|mut pipe| {
            std::thread::spawn(move || {
                let mut buffer = Vec::new();
                let _ = pipe.read_to_end(&mut buffer);
                buffer
            })
        })
    };
    let stdout = read(child.stdout.take());
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let join = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
        handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(TimedRun::Done(GitOutput {
                    status,
                    stdout: String::from_utf8_lossy(&join(stdout)).into_owned(),
                    stderr: String::from_utf8_lossy(&join(stderr)).into_owned(),
                }));
            }
            Ok(None) => {}
            Err(error) => {
                #[cfg(unix)]
                kill_group(pid);
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                let _ = join(stdout);
                let _ = join(stderr);
                return Err(RunError::Spawn(error));
            }
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            kill_group(pid);
            #[cfg(not(unix))]
            let _ = child.kill();
            let _ = child.wait();
            let _ = join(stdout);
            let _ = join(stderr);
            return Ok(TimedRun::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    // SAFETY: `kill` is a syscall with no memory operands; a negative pid signals the process
    // group whose id is `pid`, which `process_group(0)` made the command lead. A group that
    // already exited yields ESRCH, which is ignored.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// The default limit of one git call that talks to a remote: `[limits]
/// git_remote_timeout_secs` (ADR 0048 decision 7.8).
pub(crate) const DEFAULT_REMOTE_TIMEOUT_SECS: u64 = 300;

/// What a git call that talks to a remote produced.
pub(crate) enum RemoteRun {
    /// The command finished.
    Done(GitOutput),
    /// The limit ran out; the command's whole process group was killed.
    TimedOut { limit: Duration },
}

/// The limit of one git call that talks to a remote in `dir`: `[limits]
/// git_remote_timeout_secs` from the repository's `config.toml`, else the default. The file is
/// read on every call, so a change takes effect at the next git call; a file that does not
/// parse (a command that reads it again would report that) or a value below `1` falls back to
/// the default.
pub(crate) fn remote_timeout(dir: &Path) -> Duration {
    let secs = std::fs::read_to_string(dir.join(".radian").join("arc").join("config.toml"))
        .ok()
        .and_then(|text| crate::store::config::parse(&text).ok())
        .map(|config| config.limits.git_remote_timeout_secs)
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_REMOTE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Runs a git command in `dir` that talks to a remote (`fetch`, `push`, `ls-remote`), bounded
/// by [`remote_timeout`] and, when given, `also` (the worker wrapper's remaining wall time);
/// the shorter limit wins. A limit that runs out kills the command's process group (the
/// command leads its own group).
pub(crate) fn run_remote(
    dir: &Path,
    args: &[&str],
    also: Option<Duration>,
) -> Result<RemoteRun, RunError> {
    let limit = match also {
        Some(also) => remote_timeout(dir).min(also),
        None => remote_timeout(dir),
    };
    match run_with_timeout(dir, args, Some(limit))? {
        TimedRun::Done(output) => Ok(RemoteRun::Done(output)),
        TimedRun::TimedOut => Ok(RemoteRun::TimedOut { limit }),
    }
}

/// The text a remote call that ran out of time fails with: `git <subcommand> timed out after
/// <n> s`. The first argument is the subcommand of a remote call (`fetch`, `push`,
/// `ls-remote`).
pub(crate) fn timeout_text(args: &[&str], limit: Duration) -> String {
    let subcommand = args.first().copied().unwrap_or("git");
    format!("git {subcommand} timed out after {} s", limit.as_secs())
}

/// The name of the lock file, in the repository's git common dir, that serializes every fetch
/// arc runs into that repository (ADR 0048 decision 10). Every worktree of one repository
/// resolves the same common dir and so shares the lock; a fetch arc did not start, a person's
/// own `git fetch` included, is not serialized by it.
pub(crate) const FETCH_LOCK_FILE: &str = "radian-arc-fetch.lock";

/// How often a fetch looks for the shared lock while another process holds it.
const LOCK_POLL: Duration = Duration::from_millis(20);

/// What a fetch arc runs produced.
pub(crate) enum FetchRun {
    /// The fetch finished; the caller checks the exit status.
    Done(GitOutput),
    /// The limit ran out while the fetch ran; its whole process group was killed.
    TimedOut { limit: Duration },
    /// The limit ran out while waiting for the fetch lock; no git command ran.
    LockTimedOut { limit: Duration, lock: PathBuf },
}

/// Runs a fetch in `dir` under the repository's one fetch lock (ADR 0048 decision 10).
///
/// The exclusive `flock` on `<git common dir>/radian-arc-fetch.lock` is held for the fetch
/// only, and waiting for it counts against `limit`, the same wall-clock limit the fetch itself
/// has: a fetch that gives up on the lock ran no git command, and its failure names the lock.
/// Two arc processes, and two worktrees of one repository, therefore never race git's
/// compare-and-swap update of `refs/remotes/<remote>/<branch>`.
pub(crate) fn fetch(dir: &Path, args: &[&str], limit: Duration) -> Result<FetchRun, RunError> {
    let deadline = Instant::now() + limit;
    let lock = FetchLock::open(dir)?;
    if !lock.acquire(deadline)? {
        return Ok(FetchRun::LockTimedOut {
            limit,
            lock: lock.path.clone(),
        });
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Ok(FetchRun::TimedOut { limit });
    }
    match run_with_timeout(dir, args, Some(remaining))? {
        TimedRun::Done(output) => Ok(FetchRun::Done(output)),
        TimedRun::TimedOut => Ok(FetchRun::TimedOut { limit }),
    }
}

/// The lock file [`fetch`] uses in `dir`'s repository.
pub(crate) fn fetch_lock_path(dir: &Path) -> Result<PathBuf, RunError> {
    Ok(common_dir(dir)?.join(FETCH_LOCK_FILE))
}

/// The text a fetch that ran out of time while waiting for the shared lock fails with. It
/// names the lock, so the failure is not mistaken for a git error.
pub(crate) fn lock_timeout_text(lock: &Path, limit: Duration) -> String {
    format!(
        "git fetch timed out after {} s waiting for the arc fetch lock {}",
        limit.as_secs(),
        lock.display()
    )
}

/// The repository's absolute git common dir: every worktree of one repository names the same
/// directory, so every worktree shares one fetch lock.
fn common_dir(dir: &Path) -> Result<PathBuf, RunError> {
    let output = run(dir, &["rev-parse", "--git-common-dir"])?;
    if !output.success() {
        let stderr = output.stderr.trim();
        let detail = stderr.lines().last().unwrap_or(stderr);
        return Err(RunError::Lock(format!(
            "cannot find the git common dir of {}: {detail}",
            dir.display()
        )));
    }
    let text = output.stdout.trim();
    if text.is_empty() {
        return Err(RunError::Lock(format!(
            "cannot find the git common dir of {}",
            dir.display()
        )));
    }
    let path = PathBuf::from(text);
    // A plain checkout prints `.git` (relative to `dir`); a linked worktree prints the main
    // checkout's git dir. Resolving both gives one file for the whole repository.
    let path = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    std::fs::canonicalize(&path).map_err(|source| {
        RunError::Lock(format!(
            "cannot resolve the git common dir {}: {source}",
            path.display()
        ))
    })
}

/// A held `flock` on the repository's fetch lock. Dropping it releases the lock.
struct FetchLock {
    file: std::fs::File,
    path: PathBuf,
}

impl FetchLock {
    /// Opens (creating it when missing) the repository's fetch lock file.
    fn open(dir: &Path) -> Result<Self, RunError> {
        let path = fetch_lock_path(dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| {
                RunError::Lock(format!(
                    "cannot open the arc fetch lock {}: {source}",
                    path.display()
                ))
            })?;
        Ok(Self { file, path })
    }

    /// Takes the lock, waiting until `deadline`. `false` when the deadline passed first.
    fn acquire(&self, deadline: Instant) -> Result<bool, RunError> {
        loop {
            match self.try_acquire() {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(source) => {
                    return Err(RunError::Lock(format!(
                        "cannot take the arc fetch lock {}: {source}",
                        self.path.display()
                    )));
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep((deadline - now).min(LOCK_POLL));
        }
    }

    /// Tries to take the lock without waiting. `WouldBlock` means someone else holds it.
    #[cfg(unix)]
    fn try_acquire(&self) -> std::io::Result<()> {
        // SAFETY: `flock` is a syscall on `self.file`'s descriptor, which stays open; it has no
        // memory operands.
        let result = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    #[cfg(not(unix))]
    fn try_acquire(&self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the arc fetch lock needs flock(2)",
        ))
    }
}

impl Drop for FetchLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock; the explicit unlock keeps the intent.
        #[cfg(unix)]
        // SAFETY: the descriptor belongs to `self.file`, which is still open here.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Runs a fetch for the integration tests and answers the line a stage would fail with. Not
/// part of the crate's API (`git` stays private); `Ok(())` is a fetch that exited zero.
pub(crate) fn fetch_for_test(dir: &Path, args: &[&str], limit: Duration) -> Result<(), String> {
    match fetch(dir, args, limit) {
        Ok(FetchRun::Done(output)) if output.success() => Ok(()),
        Ok(FetchRun::Done(output)) => Err(format!(
            "git {} failed: {}",
            args.join(" "),
            output.stderr.trim()
        )),
        Ok(FetchRun::TimedOut { limit }) => Err(timeout_text(args, limit)),
        Ok(FetchRun::LockTimedOut { limit, lock }) => Err(lock_timeout_text(&lock, limit)),
        Err(error) => Err(error.to_string()),
    }
}

/// Runs a git command in `dir` and requires success, returning trimmed standard output.
/// A failure names the command and git's own last stderr line.
pub(crate) fn ok(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = run(dir, args).map_err(|error| error.to_string())?;
    if !output.success() {
        let stderr = output.stderr.trim();
        let detail = stderr.lines().last().unwrap_or(stderr);
        return Err(format!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(output.stdout.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A directory with an arc `config.toml` that sets the key, or none when `key` is `None`.
    fn dir_with_key(label: &str, key: Option<u64>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("radian-arc-git-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".radian").join("arc")).unwrap();
        if let Some(key) = key {
            std::fs::write(
                dir.join(".radian").join("arc").join("config.toml"),
                format!("schema_version = 1\n[limits]\ngit_remote_timeout_secs = {key}\n"),
            )
            .unwrap();
        }
        dir
    }

    /// The key bounds a remote call; a missing file, a file without the key and a `0` take
    /// the code's default.
    #[test]
    fn the_remote_timeout_is_the_key_or_the_default() {
        let default = Duration::from_secs(DEFAULT_REMOTE_TIMEOUT_SECS);
        let missing = dir_with_key("missing", None);
        assert_eq!(remote_timeout(&missing), default);
        let set = dir_with_key("set", Some(7));
        assert_eq!(remote_timeout(&set), Duration::from_secs(7));
        let zero = dir_with_key("zero", Some(0));
        assert_eq!(remote_timeout(&zero), default);
        for dir in [missing, set, zero] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The failure line names the subcommand and the limit the call ran out on.
    #[test]
    fn a_remote_timeout_names_its_subcommand_and_limit() {
        assert_eq!(
            timeout_text(&["fetch", "origin"], Duration::from_secs(1)),
            "git fetch timed out after 1 s"
        );
        assert_eq!(
            timeout_text(&["ls-remote", "origin"], Duration::from_secs(300)),
            "git ls-remote timed out after 300 s"
        );
        assert_eq!(
            timeout_text(&["push", "origin"], Duration::from_secs(7)),
            "git push timed out after 7 s"
        );
    }

    /// A local call is not bounded by the remote key: a git alias that sleeps longer than the
    /// key still finishes, while the key is short enough that a remote call would be killed.
    #[test]
    fn a_local_call_is_not_bounded_by_the_remote_key() {
        let dir = dir_with_key("local", Some(1));
        assert_eq!(remote_timeout(&dir), Duration::from_secs(1));
        let started = Instant::now();
        let output =
            run(&dir, &["-c", "alias.slow=!sleep 2", "slow"]).expect("the local command runs");
        assert!(output.success(), "{}", output.stderr);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "the local call must outlive the remote key: {:?}",
            started.elapsed()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
