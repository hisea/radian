//! The gate's git calls: the crate's one git helper (`crate::git`) with the gate's error type.
//!
//! The helper builds every command from `radian_coding::host_git::host_git_command`, adds an
//! empty hooks path and `GIT_CONFIG_NOSYSTEM=1`, sets `GIT_TERMINAL_PROMPT=0`, and clears the
//! inherited `GIT_CONFIG_*` channels and repository locators (see `crate::git`).

use std::path::Path;

use super::error::GateError;
use crate::git::RunError;

pub(crate) use crate::git::GitOutput;

/// Runs a git command in `dir` without checking the exit status.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<GitOutput, GateError> {
    crate::git::run(dir, args).map_err(|error| match error {
        RunError::Spawn(source) => GateError::Spawn {
            program: "git".to_string(),
            source,
        },
        RunError::Build(message) => GateError::Git {
            dir: dir.to_path_buf(),
            command: command_line(args),
            stderr: message,
        },
    })
}

/// Runs a git command in `dir` that talks to a remote, bounded by
/// `[limits] git_remote_timeout_secs`. A limit that runs out kills the command's process group
/// and fails with `git <subcommand> timed out after <n> s`.
pub(crate) fn run_remote(dir: &Path, args: &[&str]) -> Result<GitOutput, GateError> {
    match crate::git::run_remote(dir, args, None).map_err(|error| match error {
        RunError::Spawn(source) => GateError::Spawn {
            program: "git".to_string(),
            source,
        },
        RunError::Build(message) => GateError::Git {
            dir: dir.to_path_buf(),
            command: command_line(args),
            stderr: message,
        },
    })? {
        crate::git::RemoteRun::Done(output) => Ok(output),
        crate::git::RemoteRun::TimedOut { limit } => Err(GateError::RemoteTimeout(
            crate::git::timeout_text(args, limit),
        )),
    }
}

/// Runs a git command in `dir` that talks to a remote and requires success.
fn remote_ok(dir: &Path, args: &[&str]) -> Result<String, GateError> {
    let output = run_remote(dir, args)?;
    finish(dir, args, output)
}

/// Runs a git command in `dir` and requires success, returning trimmed standard output.
pub(crate) fn ok(dir: &Path, args: &[&str]) -> Result<String, GateError> {
    let output = run(dir, args)?;
    finish(dir, args, output)
}

fn finish(dir: &Path, args: &[&str], output: GitOutput) -> Result<String, GateError> {
    if !output.success() {
        return Err(GateError::Git {
            dir: dir.to_path_buf(),
            command: command_line(args),
            stderr: output.stderr.trim().to_string(),
        });
    }
    Ok(output.stdout.trim().to_string())
}

/// The command line shown in an error message.
pub(crate) fn command_line(args: &[&str]) -> String {
    std::iter::once("git")
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fetches `remote`. The gate never trusts a local ref alone; the remote is the truth.
pub(crate) fn fetch_remote(repository: &Path, remote: &str) -> Result<(), GateError> {
    remote_ok(repository, &["fetch", remote])?;
    Ok(())
}

/// Fetches the remote's `main_branch` into its tracking ref with an explicit refspec. A clone
/// whose configured fetch refspec leaves main out would otherwise keep a stale
/// `refs/remotes/<remote>/<main>`; the wrapper fetches its start commit the same way
/// (ADR 0048 decisions 7 and 10).
pub(crate) fn fetch_main(
    repository: &Path,
    remote: &str,
    main_branch: &str,
) -> Result<(), GateError> {
    let refspec = format!("+refs/heads/{main_branch}:refs/remotes/{remote}/{main_branch}");
    remote_ok(repository, &["fetch", remote, &refspec])?;
    Ok(())
}

/// The branch a `refs/remotes/<remote>/<branch>` ref names.
pub(crate) fn main_branch_of<'a>(remote: &str, main_ref: &'a str) -> Result<&'a str, GateError> {
    main_ref
        .strip_prefix(&format!("refs/remotes/{remote}/"))
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| GateError::MainRef(main_ref.to_owned()))
}

/// Resolves `revision` to a full commit id, or fails with [`GateError::UnknownCommit`].
pub(crate) fn resolve_commit(repository: &Path, revision: &str) -> Result<String, GateError> {
    let object = format!("{revision}^{{commit}}");
    let output = run(
        repository,
        &["rev-parse", "--verify", "--quiet", object.as_str()],
    )?;
    let sha = output.stdout.trim();
    if !output.success() || sha.is_empty() {
        return Err(GateError::UnknownCommit {
            repository: repository.to_path_buf(),
            sha: revision.to_string(),
        });
    }
    Ok(sha.to_string())
}

/// Whether `ancestor` is an ancestor of `descendant` (`git merge-base --is-ancestor`).
pub(crate) fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool, GateError> {
    let output = run(dir, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    Ok(output.success())
}
