//! Terminal state: raw mode, the alternate screen, bracketed paste, and the cursor, with a
//! guard that restores all of them on every exit path.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Whether mouse reporting is on. A process-global rather than a [`TtyGuard`] field because
/// the panic hook constructs a bare `TtyGuard` to restore the terminal and must know whether
/// to send the `l` sequences (ADR 0023 U2).
static MOUSE_REPORTING: AtomicBool = AtomicBool::new(false);

/// Whether the keyboard-enhancement flags were pushed. Global for the same reason as
/// [`MOUSE_REPORTING`]: the panic hook and the signal watchdog restore a terminal they did
/// not enter, and must pop the flags exactly when they were pushed.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// The environment variable that decides the keyboard-enhancement query:
///
/// * unset — ask the terminal (`supports_keyboard_enhancement`), which is a **2 s** wait on a
///   terminal that answers neither the kitty flags query nor the device-attributes query;
/// * `0`/`false` — never ask and never push (the pty tests set this: a pty has nobody to
///   answer, and the probe would cost 2 s per spawn);
/// * `1`/`true` — push without asking (a pty test can then assert the push/pop sequences).
pub const KEYBOARD_ENHANCEMENT_ENV: &str = "RADIAN_KEYBOARD_ENHANCEMENT";

/// What [`KEYBOARD_ENHANCEMENT_ENV`] says: `Some(true)` force on, `Some(false)` off, `None`
/// ask the terminal.
fn keyboard_enhancement_override() -> Option<bool> {
    match std::env::var(KEYBOARD_ENHANCEMENT_ENV).ok().as_deref() {
        Some("0") | Some("false") => Some(false),
        Some("1") | Some("true") => Some(true),
        _ => None,
    }
}

/// Whether the terminal should get the keyboard-enhancement flags: the override when set,
/// otherwise the terminal's own answer.
///
/// `supports_keyboard_enhancement` (crossterm 0.29.0, `terminal/sys/unix.rs:188-263`) writes
/// `ESC[?u ESC[c` to `/dev/tty` (stdout when that cannot be opened) and polls for the kitty
/// flags reply for up to **2000 ms**. A terminal that answers the device-attributes query but
/// not the flags query makes it return `Ok(None)` quickly (the reply ends the wait), so a
/// modern terminal pays milliseconds; a terminal that answers neither pays the full 2 s and
/// comes back as `Err`, which is treated as "unsupported" - never as a failure (ADR 0009: no
/// correctness depends on a terminal's reply). It must run **before** the input thread reads:
/// crossterm documents that the query blocks while `event::read`/`poll` are in progress, and
/// both would race for the same bytes.
fn should_enhance_keyboard() -> bool {
    match keyboard_enhancement_override() {
        Some(force) => force,
        None => {
            use ratatui::crossterm::terminal::supports_keyboard_enhancement;
            supports_keyboard_enhancement().unwrap_or(false)
        }
    }
}

/// Turns mouse reporting on (`\x1b[?1000h\x1b[?1002h\x1b[?1006h`) or off (the `l` forms,
/// reverse order) on `out`.
///
/// Deliberately **not** crossterm's `EnableMouseCapture`: that also enables `?1003h`
/// (any-motion), so a pointer move would flood the input thread (crossterm 0.29.0
/// `event.rs:325-333`). We need `?1000h` (button + wheel presses), `?1002h` (motion while a
/// button is held: the drag of a transcript selection) and `?1006h` (SGR encoding, so the
/// coordinates do not overflow past column 223). Motion **without** a button is not reported
/// under `?1002h`, and `?1003h` stays off, so an idle pointer produces no events and no
/// frames (ADR 0023 U2's zero-redraw). Shift+button is still left to the terminal (most do
/// not report it), so the native selection remains the fallback.
fn write_mouse(out: &mut impl Write, enabled: bool) -> io::Result<()> {
    if enabled {
        out.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    } else {
        out.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l")?;
    }
    out.flush()
}

/// Undoes every terminal modification, writing to `out`: mouse reporting off (only when it
/// was on), cursor shown, bracketed paste off, leave the alternate screen, cooked mode, and a
/// trailing newline. Best-effort: every write failure is ignored, because the terminal may
/// already be gone. **One** copy of the escape sequences, shared by the guard, the panic hook
/// and the signal watchdog.
fn restore_to(out: &mut impl Write) {
    if KEYBOARD_ENHANCED.swap(false, Ordering::SeqCst) {
        // Popped before the alternate screen is left, like the mouse sequences.
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if MOUSE_REPORTING.swap(false, Ordering::SeqCst) {
        let _ = write_mouse(out, false);
    }
    let _ = execute!(out, Show, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = out.flush();
    let _ = disable_raw_mode();
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
}

/// Restores the terminal on the ordinary exit paths (the guard's `Drop`, the panic hook).
pub fn restore_terminal() {
    restore_to(&mut io::stdout());
}

/// The signal watchdog's restore: the main loop may be stuck mid-write with the terminal's
/// output buffer full, and it may still hold Rust's stdout lock, so going through
/// `io::stdout()` could block forever and the forced exit would never happen. Write the same
/// [`restore_to`] sequences straight to fd 1, after making it non-blocking: no lock to wait
/// on, and a failed or partial write is ignored. This is best-effort, the process is leaving.
///
/// `O_NONBLOCK` is a **status flag of the open file description**, which this process shares
/// with the shell and every later program on the same terminal (a `dup` shares it; only a fresh
/// `open` does not). Leaving it set would make their stdout writes fail with `EAGAIN`
/// ("Resource temporarily unavailable") and truncate output. So the original flags are saved,
/// `O_NONBLOCK` is set, the restore is written, and the original flags are put back before the
/// caller exits. If `F_GETFL` fails, non-blocking is never set.
pub fn restore_terminal_nonblocking() {
    #[cfg(unix)]
    {
        // `libc::STDOUT_FILENO`, never `io::stdout()`: the latter can wait on the buffer lock
        // the stuck main loop is holding.
        let fd = libc::STDOUT_FILENO;
        // SAFETY: `fd` is our own stdout and `fcntl` only reads/writes its flags.
        let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original >= 0 {
            // SAFETY: same fd and flag-only mutation.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original | libc::O_NONBLOCK) };
        }
        restore_to(&mut RawFdWriter { fd });
        if original >= 0 {
            // Put the shared description back the way we found it. Best-effort too: a failure
            // here must not stop the forced exit, but it must never be skipped.
            // SAFETY: same fd and flag-only mutation.
            unsafe { libc::fcntl(fd, libc::F_SETFL, original) };
        }
    }
    #[cfg(not(unix))]
    {
        restore_terminal();
    }
}

/// A lock-free `Write` over a raw fd, so the signal watchdog's restore never waits on the
/// stdout buffer lock a stuck frame may be holding.
#[cfg(unix)]
struct RawFdWriter {
    fd: std::os::fd::RawFd,
}

#[cfg(unix)]
impl Write for RawFdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: `buf` is a valid slice and `self.fd` is our own stdout.
        let written = unsafe { libc::write(self.fd, buf.as_ptr().cast(), buf.len()) };
        if written < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(written as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Owns the terminal's modified state. `Drop` restores it, so a normal return, an error
/// return and a panic unwind all restore; a `panic::set_hook` covers the process abort path.
pub struct TtyGuard {
    active: bool,
}

impl TtyGuard {
    /// Enters raw mode and the alternate screen. `mouse` says whether to also turn mouse
    /// reporting on (`tui.mouse`, default on): `false` sends no mouse sequence at all, so the
    /// terminal keeps its native selection/copy and behaves exactly as before.
    pub fn enter(mouse: bool) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        // If entering the alternate screen fails, put the terminal back before returning:
        // there is no guard yet for `Drop` to restore (dogfooding D4's exit-path audit).
        if let Err(error) = execute!(out, EnterAlternateScreen, EnableBracketedPaste, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        if mouse {
            // Set the flag before the write: if the write fails partway, `restore` still
            // sends the `l` forms, which are harmless when the `h` never took effect.
            MOUSE_REPORTING.store(true, Ordering::SeqCst);
            if let Err(error) = write_mouse(&mut out, true) {
                let mut guard = Self { active: true };
                guard.restore();
                return Err(error);
            }
        }
        // The keyboard-enhancement query runs **here**, before the caller spawns the input
        // thread (see `should_enhance_keyboard`): it reads the terminal's reply, and an input
        // thread reading at the same time would race it. `DISAMBIGUATE_ESCAPE_CODES` is what
        // makes Shift+Enter arrive as Enter+SHIFT instead of a plain Enter.
        if should_enhance_keyboard() {
            KEYBOARD_ENHANCED.store(true, Ordering::SeqCst);
            if let Err(error) = execute!(
                out,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            ) {
                let mut guard = Self { active: true };
                guard.restore();
                return Err(error);
            }
        }
        Ok(Self { active: true })
    }

    /// Restores the terminal. Idempotent.
    pub fn restore(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        restore_terminal();
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

/// Whether the terminal cursor is currently shown. `enter` hides it, and the app shows it only
/// while the input box is editable, so the cursor is never parked in the transcript or over an
/// approval box.
static CURSOR_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Shows or hides the terminal cursor, emitting the escape only on a change. The event loop
/// calls this once per frame with [`crate::App::input_editable`].
pub fn set_cursor_visible(visible: bool) {
    use std::sync::atomic::Ordering;
    if CURSOR_SHOWN.swap(visible, Ordering::SeqCst) == visible {
        return;
    }
    let mut out = io::stdout();
    let _ = if visible {
        execute!(out, Show)
    } else {
        execute!(out, Hide)
    };
    let _ = out.flush();
}

/// Where a panic is also appended, and the values that must not appear there. Set once the
/// session id and directory are known (dogfooding D4).
static PANIC_LOG: Mutex<Option<PanicLog>> = Mutex::new(None);

/// The session's shared redaction list: header values and every key in `auth.json`. `/login`
/// appends to it when it saves a key mid-session, and both the
/// panic hook and an export read it live; nothing ever removes an entry, so a value that
/// appeared stays redacted even after `/logout`. A value this list cannot carry — a `chatgpt`
/// token refreshed mid-session (`radian_host::secrets`) — is merged in by [`shared_secrets`]
/// on every read instead.
pub type SharedSecrets = std::sync::Arc<std::sync::RwLock<Vec<String>>>;

struct PanicLog {
    path: PathBuf,
    /// Header values and the API keys: the panic text must not carry them into the log.
    secrets: SharedSecrets,
}

/// Snapshot the shared list **plus every value registered since it was built**. Every reader
/// goes through here — the panic hook, the model-error line and an export/picker — so the
/// merge lives here too.
///
/// **Poison-safe**: a panic while a writer held the lock must not turn redaction off (failing
/// open is the wrong direction for a redaction list), so the guard is taken through
/// `into_inner` instead of being dropped.
///
/// The registry part is not optional: `resolved_keys` was read once, at startup, and a
/// `chatgpt` access token that expires mid-session is refreshed by `ensure_fresh` and written
/// to `auth.json` after that (ADR 0026 — an OAuth token is redacted
/// exactly like an API key). A copy taken at startup would stop covering the token in use about
/// an hour into the session; `/login` covers its own case by appending to this list.
pub fn shared_secrets(secrets: &SharedSecrets) -> Vec<String> {
    let mut list = secrets
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    for value in radian_host::registered_secrets() {
        if !list.contains(&value) {
            list.push(value);
        }
    }
    list
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
/// stored summary and the log line. `None` (no diagnostics directory) disables the log.
pub fn set_panic_log(path: Option<PathBuf>, secrets: SharedSecrets) {
    let mut slot = PANIC_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = path.map(|path| PanicLog { path, secrets });
}

/// Where the panic log is being written, for the exit line (dogfooding D4).
pub fn panic_log_path() -> Option<PathBuf> {
    PANIC_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|log| log.path.clone())
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
    let (path, secrets) = {
        // Poison-tolerant too: a panicked holder of `PANIC_LOG` must not drop the list.
        let slot = PANIC_LOG
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match slot.as_ref() {
            Some(log) => (Some(log.path.clone()), shared_secrets(&log.secrets)),
            None => (None, Vec::new()),
        }
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
        // The diagnostics directory (ADR 0013 decision 4): owner-only, cleaned before the
        // write, and a failure is one line rather than a fault.
        if let Err(error) = radian_host::append(&path, &line) {
            eprintln!("[diagnostics] cannot write {}: {error}", path.display());
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
            restore_terminal();
        }
        let _ = writeln!(io::stderr(), "{report}");
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A poisoned secrets lock must still supply the list: redaction must never fail open
    ///. The lock is poisoned by a thread that panics while
    /// holding the write guard, which is exactly how a mid-panic writer poisons it.
    #[test]
    fn a_poisoned_shared_secrets_list_still_redacts() {
        const KEY: &str = "late-secret-PTY-7";
        let secrets: SharedSecrets =
            std::sync::Arc::new(std::sync::RwLock::new(vec![KEY.to_string()]));

        let poison = std::sync::Arc::clone(&secrets);
        let _ = std::thread::spawn(move || {
            let _guard = poison.write().unwrap_or_else(|e| e.into_inner());
            panic!("poison the lock");
        })
        .join();
        assert!(
            secrets.is_poisoned(),
            "the lock must be poisoned for the test"
        );

        // The redaction read path: snapshot the list, then redact with it.
        let list = shared_secrets(&secrets);
        assert_eq!(
            list,
            vec![KEY.to_string()],
            "a poisoned lock must not empty the list"
        );
        let redacted = redact(&format!("the key is {KEY}"), &list);
        assert!(redacted.contains("<redacted>"), "{redacted}");
        assert!(!redacted.contains(KEY), "{redacted}");
    }

    /// The write side is poison-tolerant too: a poisoned lock does not stop an append (the
    /// list only grows; failing to add a key is the dangerous direction).
    #[test]
    fn an_append_through_a_poisoned_lock_still_lands() {
        let secrets: SharedSecrets = std::sync::Arc::new(std::sync::RwLock::new(vec![]));
        let poison = std::sync::Arc::clone(&secrets);
        let _ = std::thread::spawn(move || {
            let _guard = poison.write().unwrap_or_else(|e| e.into_inner());
            panic!("poison the lock");
        })
        .join();
        {
            let mut list = secrets
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            list.push("added-after-poison".to_string());
        }
        assert_eq!(
            shared_secrets(&secrets),
            vec!["added-after-poison".to_string()]
        );
    }
}
