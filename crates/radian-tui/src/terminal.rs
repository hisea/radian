//! Terminal state: raw mode, the alternate screen, bracketed paste, and the cursor, with a
//! guard that restores all of them on every exit path.

use std::io::{self, Write};
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
        execute!(out, EnterAlternateScreen, EnableBracketedPaste, Hide)?;
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

/// The most recent panic message, so `main` can print it after the alternate screen is gone
/// (a panic on a spawned task writes into the alternate screen, where nobody will see it).
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// The message of the last panic, if any.
pub fn last_panic() -> Option<String> {
    LAST_PANIC.lock().ok().and_then(|slot| slot.clone())
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
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(info.to_string());
        }
        if std::thread::current().name() == Some("main") {
            let mut guard = TtyGuard { active: true };
            guard.restore();
        }
        default(info);
    }));
}
