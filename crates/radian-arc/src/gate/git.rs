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
    ok(repository, &["fetch", remote])?;
    Ok(())
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
