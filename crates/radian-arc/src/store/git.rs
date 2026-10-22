//! The few git commands arc runs itself.
//!
//! Every command is built from `radian_coding::host_git::host_git_command`, the one place a
//! host `git` call is built: it carries the `-c` overrides that neutralise the repository's
//! own filters, hooks, fsmonitor and diff drivers, so no file a person or a worker can write
//! changes what arc runs. On top of that each call sets `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_TERMINAL_PROMPT=0`: the system configuration does not take part, and a missing
//! credential fails at once instead of waiting on a prompt.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The command for one git call in `dir`, with the shared overrides and arc's own
/// environment.
fn command(dir: &Path) -> Result<Command, String> {
    let mut command =
        radian_coding::host_git::host_git_command(dir).map_err(|error| format!("git: {error}"))?;
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    Ok(command)
}

/// Runs git in `dir` and returns its trimmed stdout. A non-zero exit becomes one line
/// holding git's own last stderr line.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = command(dir)?
        .args(args)
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("exit {}", output.status)
        } else {
            stderr.lines().last().unwrap_or(stderr).to_owned()
        };
        return Err(format!("git {}: {detail}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The repository root that contains `dir`.
pub(crate) fn repository_root(dir: &Path) -> Result<PathBuf, String> {
    let root = run(dir, &["rev-parse", "--show-toplevel"])?;
    if root.is_empty() {
        return Err(format!(
            "{} is not inside a git working tree",
            dir.display()
        ));
    }
    std::fs::canonicalize(&root)
        .map_err(|error| format!("cannot resolve the repository root `{root}`: {error}"))
}

/// The URL of `remote`.
pub(crate) fn remote_url(dir: &Path, remote: &str) -> Result<String, String> {
    run(dir, &["remote", "get-url", remote])
}

/// The branch a task starts from: the remote's own HEAD (`origin/HEAD`) when it is known,
/// else the checked-out branch, else `main`.
pub(crate) fn main_branch(dir: &Path) -> String {
    if let Ok(reference) = run(
        dir,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) && let Some(branch) = reference.strip_prefix("origin/")
        && !branch.is_empty()
    {
        return branch.to_owned();
    }
    if let Ok(branch) = run(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        && !branch.is_empty()
    {
        return branch;
    }
    "main".to_owned()
}

/// The repository's local exclude file (`.git/info/exclude`, or the common directory's in a
/// linked worktree).
pub(crate) fn exclude_path(dir: &Path) -> Result<PathBuf, String> {
    let path = run(dir, &["rev-parse", "--git-path", "info/exclude"])?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// Whether the remote answers (`git ls-remote`).
pub(crate) fn ls_remote(dir: &Path, remote: &str) -> Result<(), String> {
    run(dir, &["ls-remote", "--exit-code", remote, "HEAD"]).map(|_| ())
}

/// Whether a push to `remote`'s `branch` would be accepted, without changing the remote.
pub(crate) fn push_dry_run(dir: &Path, remote: &str, branch: &str) -> Result<(), String> {
    let refspec = format!("{branch}:{branch}");
    run(dir, &["push", "--dry-run", remote, &refspec]).map(|_| ())
}
