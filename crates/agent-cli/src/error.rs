//! Why startup or a "print and exit" command stopped (ADR 0032): the error types
//! behind `radian exec`'s exit codes.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use radian_host::memory::commands::CommandError;

use super::USAGE_ERROR;

/// Why startup failed. The message must not contain the key.
pub(crate) struct SetupError(pub(crate) String);

/// Why a "print and exit" command (`--list`, `--search`, `--export`, `radian memory`) stopped.
///
/// A closed stdout (`radian exec --list | head`) is a normal way to stop, not a failure
/// (dogfooding
/// D21); it is kept apart from a real error so it exits 0 with no message.
pub(crate) enum OutputError {
    /// A setup, usage or memory-command failure: the message is printed, this code returned.
    Failed { message: String, code: i32 },
    /// stdout's reader went away: stop silently with exit code 0.
    BrokenPipe,
    /// Another stdout write error: one stderr line, exit code 1.
    Write(std::io::Error),
}

impl From<SetupError> for OutputError {
    fn from(error: SetupError) -> Self {
        Self::Failed {
            message: error.0,
            code: USAGE_ERROR,
        }
    }
}

impl From<CommandError> for OutputError {
    fn from(error: CommandError) -> Self {
        Self::Failed {
            message: error.message,
            code: error.code,
        }
    }
}

impl From<radian_host::PiMigrateError> for OutputError {
    fn from(error: radian_host::PiMigrateError) -> Self {
        Self::Failed {
            message: error.to_string(),
            code: 1,
        }
    }
}

impl From<radian_host::ClaudeMigrateError> for OutputError {
    fn from(error: radian_host::ClaudeMigrateError) -> Self {
        let code = match error {
            radian_host::ClaudeMigrateError::MissingSource => 2,
            _ => 1,
        };
        Self::Failed {
            message: error.to_string(),
            code,
        }
    }
}

impl From<radian_host::StdoutError> for OutputError {
    fn from(error: radian_host::StdoutError) -> Self {
        match error {
            radian_host::StdoutError::BrokenPipe => Self::BrokenPipe,
            radian_host::StdoutError::Failed(error) => Self::Write(error),
        }
    }
}

/// The shared `--list` (ADR 0032 slice 5). An index failure is this host's usage error (2), a
/// stdout write failure its exit 1, and a closed reader its silent stop.
impl From<radian_host::ListError> for OutputError {
    fn from(error: radian_host::ListError) -> Self {
        match error {
            radian_host::ListError::Failed(message) => SetupError(message).into(),
            radian_host::ListError::Write(error) => Self::Write(error),
            radian_host::ListError::BrokenPipe => Self::BrokenPipe,
        }
    }
}

impl OutputError {
    /// The process exit code for this failure.
    fn exit_code(&self) -> i32 {
        match self {
            OutputError::Failed { code, .. } => *code,
            OutputError::BrokenPipe => 0,
            OutputError::Write(_) => 1,
        }
    }

    /// The line to print to stderr, if any. A broken pipe prints nothing.
    fn message(&self) -> Option<String> {
        match self {
            OutputError::Failed { message, .. } => Some(format!("radian: {message}")),
            OutputError::BrokenPipe => None,
            OutputError::Write(error) => Some(format!("radian: cannot write to stdout: {error}")),
        }
    }
}

/// Prints an output command's failure (if any) and returns its exit code.
pub(crate) fn finish(result: Result<(), OutputError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            if let Some(line) = error.message() {
                eprintln!("{line}");
            }
            error.exit_code()
        }
    }
}
