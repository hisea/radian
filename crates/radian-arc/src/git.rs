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
//! The repository, global and system configuration files are otherwise unaffected.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The environment variables that would point git at another repository or object store.
const REPOSITORY_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

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
pub(crate) enum RunError {
    /// The shared builder could not read the effective configuration.
    Build(String),
    /// The process could not be started.
    Spawn(std::io::Error),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Build(message) => f.write_str(message),
            RunError::Spawn(error) => write!(f, "cannot run git: {error}"),
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
/// waits on a remote cannot outlive the attempt. `None` waits forever, which is what every
/// non-wrapper caller wants.
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
