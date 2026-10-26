//! Errors of the gate library. Every git, file-system and process failure keeps the path or
//! command that failed, so a verdict's `reason` never has to guess.

use std::path::PathBuf;

/// One gate failure.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// A file-system operation failed.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path the operation touched.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A git command exited non-zero.
    #[error("git {command} failed in {dir}: {stderr}")]
    Git {
        /// The directory the command ran in.
        dir: PathBuf,
        /// The command line, joined for the message.
        command: String,
        /// The command's standard error, trimmed.
        stderr: String,
    },

    /// A git command that talks to a remote ran past `[limits] git_remote_timeout_secs` and
    /// was killed with its process group. The text is `git <subcommand> timed out after <n> s`.
    #[error("{0}")]
    RemoteTimeout(String),

    /// The commit a stage is prepared for does not exist after the fetch.
    #[error("{sha} is not a commit in {repository}")]
    UnknownCommit {
        /// The repository root.
        repository: PathBuf,
        /// The unresolvable revision.
        sha: String,
    },

    /// The gate checkout path exists but is not a linked worktree, so it is not ours to reuse.
    #[error("gate checkout {0} exists and is not a linked worktree")]
    CheckoutOccupied(PathBuf),

    /// A verdict file already exists. A verdict is never changed (ADR 0048 decision 3).
    #[error("verdict {0} already exists")]
    VerdictExists(PathBuf),

    /// A verdict file could not be decoded.
    #[error("verdict {path} is not a verdict: {source}")]
    MalformedVerdict {
        /// The verdict file.
        path: PathBuf,
        /// The decode error.
        #[source]
        source: serde_json::Error,
    },

    /// A path component is not usable as one directory name.
    #[error("invalid id {0:?}: an id becomes one path component")]
    InvalidId(String),

    /// A path could not be represented as UTF-8 for a child process argument.
    #[error("path {0} is not valid UTF-8")]
    NonUtf8Path(PathBuf),

    /// The main ref does not have the shape `refs/remotes/<remote>/<branch>`.
    #[error("{0} is not a `refs/remotes/<remote>/<branch>` ref")]
    MainRef(String),

    /// A command could not be started.
    #[error("could not run {program}: {source}")]
    Spawn {
        /// The program that failed to start.
        program: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A profile lists a stage that profiles cannot list.
    #[error("stage {0} cannot be listed in a profile: it always runs at merge")]
    UnlistedStage(crate::gate::Stage),
    /// A profile lists the same stage twice.
    #[error("stage {0} is listed twice")]
    DuplicateStage(crate::gate::Stage),

    /// A profile lists a stage before the stage it depends on.
    #[error("stage {stage} must come after {after} in a profile")]
    StageOrder {
        /// The stage that is too early.
        stage: crate::gate::Stage,
        /// The stage it must follow.
        after: crate::gate::Stage,
    },

    /// A profile lists a `before_attempt` step this build does not run.
    #[error(
        "`{0}` is not a step a profile can run before an attempt; the only built-in step is \
         `merge-main`, and a custom command is a table with `run`"
    )]
    UnknownBeforeAttemptStep(String),

    /// A profile lists a `before_attempt` command without a command line.
    #[error("a `before_attempt` command has an empty `run`; a command line is required")]
    EmptyBeforeAttemptRun,

    /// A `serve` stop reached the gate: the stage child was killed and no verdict is
    /// written. The next loop starts the stage again as unverified (ADR 0048 decision 13).
    #[error("the gate was stopped")]
    Stopped,
}
