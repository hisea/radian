//! Terminal state: raw mode, the alternate screen, bracketed paste, and the cursor, with a
//! guard that restores all of them on every exit path.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Owns the terminal's modified state. `Drop` restores it, so a normal return, an error
/// return and a panic unwind all restore; a `panic::set_hook` covers the process abort path.
pub struct TtyGuard {
    active: bool,
}

impl TtyGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        // If entering the alternate screen fails, put the terminal back before returning:
        // there is no guard yet for `Drop` to restore (dogfooding D4's exit-path audit).
        if let Err(error) = execute!(out, EnterAlternateScreen, EnableBracketedPaste, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        Ok(Self { active: true })
    }

    /// Restores the terminal. Idempotent.
    pub fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let mut out = io::stdout();
        let _ = execute!(out, Show, DisableBracketedPaste, LeaveAlternateScreen);
        let _ = out.flush();
        let _ = disable_raw_mode();
        // The full-screen spike left no trailing newline, so zsh printed its "no newline"
        // `%` marker. End our output with one after leaving the alternate screen.
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The most recent panic summary, so `main` can print it after the alternate screen is gone
/// (a panic on a spawned task writes into the alternate screen, where nobody will see it).
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Where a panic is also appended, and the values that must not appear there. Set once the
/// session id and directory are known (dogfooding D4).
static PANIC_LOG: Mutex<Option<PanicLog>> = Mutex::new(None);

struct PanicLog {
    path: PathBuf,
    /// Header values and the API key: the panic text must not carry them into the log.
    secrets: Vec<String>,
}

/// The message of the last panic, if any, collapsed to one line (the terminal gets one line;
/// the multi-line detail lives in the panic log).
///
/// The summary is already redacted because [`record_panic`] stored the redacted text.
pub fn last_panic() -> Option<String> {
    LAST_PANIC.lock().ok().and_then(|slot| slot.clone())
}

/// Records where a panic should also be written. Called once per session, before the
/// terminal is entered. `secrets` (header values, the API key) are redacted out of both the
/// stored summary and the log line.
pub fn set_panic_log(path: PathBuf, secrets: Vec<String>) {
    if let Ok(mut slot) = PANIC_LOG.lock() {
        *slot = Some(PanicLog { path, secrets });
    }
}

/// Replaces every value in `secrets` with `<redacted>`. Used for panic text and for the
/// model error the driver reports (ADR 0017).
pub fn redact(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret.as_str(), "<redacted>");
        }
    }
    out
}

/// Records the panic summary (for the exit line) and appends a timestamped line to the panic
/// log, if one was set, and returns the redacted report for stderr. Never panics itself: a
/// panic hook that panicked would abort.
fn record_panic(info: &std::panic::PanicHookInfo<'_>) -> String {
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_owned();
    let full = info.to_string();
    let (path, secrets) = match PANIC_LOG.lock() {
        Ok(slot) => match slot.as_ref() {
            Some(log) => (Some(log.path.clone()), log.secrets.clone()),
            None => (None, Vec::new()),
        },
        Err(_) => (None, Vec::new()),
    };
    let redacted = redact(&full, &secrets);
    // The exit line is one line: keep the first line of the panic report.
    let summary = redacted.lines().next().unwrap_or("").to_owned();
    if let Ok(mut slot) = LAST_PANIC.lock() {
        *slot = Some(summary);
    }
    if let Some(path) = path {
        let line = format!(
            "{} thread={} {}\n",
            radian_session::now_utc(),
            thread,
            redacted.replace('\n', " ")
        );
        if let Some(parent) = path.parent() {
            // Owner-only diagnostics directory and file (ADR 0013): the panic text can carry
            // a header value or the key, so both are private.
            let _ = radian_session::ensure_private_dir(parent);
        }
        if let Ok(mut file) = radian_session::open_private_append(&path) {
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
    format!("thread '{thread}' {redacted}")
}

/// Installs a panic hook that restores the terminal **only when the panicking thread is the
/// one whose unwind takes the process down**, and otherwise just records the message.
///
/// Why: the driver runs as a spawned task. A panic inside it is caught by the runtime, so the
/// process keeps running and the UI still owns the terminal — restoring it there would put the
/// terminal in cooked mode under a UI that keeps drawing and reading keys. The main thread is
/// the process-going-down case (its unwind drops the `TtyGuard` anyway; restoring here just
/// makes the panic visible on the normal screen before that).
///
/// The hook never exits: `main` catches the unwind, shuts the runtime down with a bound
/// (dropping in-flight tool futures and their `kill_on_drop` children) and exits 101.
///
/// It replaces Rust's default hook rather than chaining to it: the default prints the raw
/// panic text to stderr, and that text is not redacted (a panic message can carry a header
/// value or the key). What reaches stderr is the same redacted text the panic log gets. The
/// cost is the default hook's backtrace note; `RUST_BACKTRACE` is not honoured here.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(move |info| {
        let report = record_panic(info);
        if std::thread::current().name() == Some("main") {
            let mut guard = TtyGuard { active: true };
            guard.restore();
        }
        let _ = writeln!(io::stderr(), "{report}");
    }));
}
