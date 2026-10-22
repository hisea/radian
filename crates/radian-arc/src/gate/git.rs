//! The gate's private git helper.
//!
//! Every git command runs as `git -c core.hooksPath= …` with `GIT_CONFIG_NOSYSTEM=1`, so a
//! `core.hooksPath` written in the repository (or in the system config) never runs a hook
//! for a gate operation (ADR 0048 decisions 7 and 10). Unattended commands also set
//! `GIT_TERMINAL_PROMPT=0`, so a missing credential fails the fetch instead of prompting.
//!
//! The inherited `GIT_CONFIG_COUNT` / `GIT_CONFIG_PARAMETERS` channels are cleared: they are
//! a caller's injected configuration, and a `core.hooksPath` there would silently replace the
//! empty one this helper asks for. The repository, global and system configuration files are
//! unaffected.
//!
//! The inherited repository locators are cleared too (`GIT_DIR`, `GIT_WORK_TREE`,
//! `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`,
//! `GIT_ALTERNATE_OBJECT_DIRECTORIES`): a caller that has one of them set would otherwise run
//! every gate command against another repository, or read objects from one. Each command names
//! its repository by its working directory, so none of them may leak in.

use std::path::Path;
use std::process::{Command, ExitStatus};

use super::error::GateError;

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
    pub(crate) fn success(&self) -> bool {
        self.status.success()
    }
}

/// The environment variables that would point git at another repository or object store.
const REPOSITORY_ENV: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

/// Runs a git command in `dir` without checking the exit status.
pub(crate) fn run(dir: &Path, args: &[&str]) -> Result<GitOutput, GateError> {
    run_with_env(dir, args, &[])
}

/// Runs a git command in `dir` with extra environment variables, without checking the exit
/// status.
pub(crate) fn run_with_env(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<GitOutput, GateError> {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("core.hooksPath=");
    cmd.args(args);
    cmd.current_dir(dir);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env_remove("GIT_CONFIG_COUNT");
    cmd.env_remove("GIT_CONFIG_PARAMETERS");
    for name in REPOSITORY_ENV {
        cmd.env_remove(name);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().map_err(|source| GateError::Spawn {
        program: "git".to_string(),
        source,
    })?;
    Ok(GitOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
