//! Writing a command's stdout, where a closed reader is a normal stop, not a crash
//! (dogfooding D21).
//!
//! `agent --export <id> | head -1` closes stdout after one line. A `println!` then panics
//! ("failed printing to stdout: Broken pipe") and the process exits 101. Restoring the
//! process's default `SIGPIPE` disposition is deliberately **not** done: writing to an MCP
//! child's pipe after the child exited would then kill the whole agent with a signal. So
//! every "print and exit" command writes through here and treats `BrokenPipe` as "the reader
//! is gone, stop quietly with exit code 0".

use std::io::Write;

/// What happened when writing to stdout.
#[derive(Debug)]
pub enum StdoutError {
    /// The reader went away (`... | head`): stop quietly with exit code 0.
    BrokenPipe,
    /// Any other write error: one line to stderr, exit code 1.
    Failed(std::io::Error),
}

impl std::fmt::Display for StdoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdoutError::BrokenPipe => f.write_str("the output pipe was closed"),
            StdoutError::Failed(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StdoutError {}

/// Writes `text` to stdout and flushes it.
pub fn write_stdout(text: &str) -> Result<(), StdoutError> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(text.as_bytes())
        .and_then(|()| lock.flush())
        .map_err(map)
}

/// Writes one line (adding the trailing newline).
pub fn write_line(line: &str) -> Result<(), StdoutError> {
    write_stdout(&format!("{line}\n"))
}

fn map(error: std::io::Error) -> StdoutError {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        StdoutError::BrokenPipe
    } else {
        StdoutError::Failed(error)
    }
}
