//! The git queries the store needs: the repository root, the remote, the main branch, the
//! exclude file, and the two `init` checks.
//!
//! Every call goes through the crate's one git helper (`crate::git`), which carries the
//! shared no-filter/no-hook overrides, an empty hooks path, `GIT_CONFIG_NOSYSTEM=1` and
//! `GIT_TERMINAL_PROMPT=0`.

use std::path::{Path, PathBuf};

use crate::git;

/// The repository root that contains `dir`.
pub(crate) fn repository_root(dir: &Path) -> Result<PathBuf, String> {
    let root = git::ok(dir, &["rev-parse", "--show-toplevel"])?;
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
    git::ok(dir, &["remote", "get-url", remote])
}

/// The branch a task starts from: the remote's own HEAD (`<remote>/HEAD`) when it is known,
/// else the checked-out branch, else `main`.
pub(crate) fn main_branch(dir: &Path, remote: &str) -> String {
    let head = format!("refs/remotes/{remote}/HEAD");
    if let Ok(reference) = git::ok(dir, &["symbolic-ref", "--quiet", "--short", &head])
        && let Some(branch) = reference.strip_prefix(&format!("{remote}/"))
        && !branch.is_empty()
    {
        return branch.to_owned();
    }
    if let Ok(branch) = git::ok(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        && !branch.is_empty()
    {
        return branch;
    }
    "main".to_owned()
}

/// The repository's local exclude file (`.git/info/exclude`, or the common directory's in a
/// linked worktree).
pub(crate) fn exclude_path(dir: &Path) -> Result<PathBuf, String> {
    let path = git::ok(dir, &["rev-parse", "--git-path", "info/exclude"])?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        dir.join(path)
    })
}

/// Whether the remote answers (`git ls-remote`). The call talks to the remote, so
/// `[limits] git_remote_timeout_secs` bounds it.
pub(crate) fn ls_remote(dir: &Path, remote: &str) -> Result<(), String> {
    remote_ok(dir, &["ls-remote", "--exit-code", remote, "HEAD"]).map(|_| ())
}

/// Whether a push to `remote`'s `branch` would be accepted, without changing the remote. The
/// call talks to the remote, so `[limits] git_remote_timeout_secs` bounds it.
pub(crate) fn push_dry_run(dir: &Path, remote: &str, branch: &str) -> Result<(), String> {
    let refspec = format!("{branch}:{branch}");
    remote_ok(dir, &["push", "--dry-run", remote, &refspec]).map(|_| ())
}

/// Runs a git command that talks to a remote and requires success, with [`git::ok`]'s failure
/// text. A call that runs past `[limits] git_remote_timeout_secs` fails with its own line.
fn remote_ok(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = match git::run_remote(dir, args, None).map_err(|error| error.to_string())? {
        git::RemoteRun::Done(output) => output,
        git::RemoteRun::TimedOut { limit } => return Err(git::timeout_text(args, limit)),
    };
    if !output.success() {
        let stderr = output.stderr.trim();
        let detail = stderr.lines().last().unwrap_or(stderr);
        return Err(format!("git {} failed: {detail}", args.join(" ")));
    }
    Ok(output.stdout.trim().to_owned())
}
