//! Terminal state: raw mode, the alternate screen, bracketed paste, and the cursor, with a
//! guard that restores all of them on every exit path.

use std::io::{self, Write};

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

/// Installs a panic hook that restores the terminal before the default hook prints. The
/// hook owns a fresh guard because the real one is a local in `main` and may not be
/// reachable from the panicking frame.
///
/// It deliberately does **not** exit. `main` catches the unwind, shuts the runtime down with
/// a bound (which drops the in-flight tool futures and the `kill_on_drop` child processes
/// they own) and then exits 101. Exiting here would skip that cleanup and orphan any child
/// process the turn had started.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut guard = TtyGuard { active: true };
        guard.restore();
        default(info);
    }));
}
