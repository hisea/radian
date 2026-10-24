//! Terminal state: raw mode, the alternate screen, bracketed paste, and the cursor, with a
//! guard that restores all of them on every exit path.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui_image::FontSize;
use ratatui_image::picker::cap_parser::{Parser as CapParser, QueryStdioOptions, Response};
use ratatui_image::picker::{Picker, ProtocolType};

/// Whether mouse reporting is on. A process-global rather than a [`TtyGuard`] field because
/// the panic hook constructs a bare `TtyGuard` to restore the terminal and must know whether
/// to send the `l` sequences (ADR 0023 U2).
static MOUSE_REPORTING: AtomicBool = AtomicBool::new(false);

/// Whether the keyboard-enhancement flags were pushed. Global for the same reason as
/// [`MOUSE_REPORTING`]: the panic hook and the signal watchdog restore a terminal they did
/// not enter, and must pop the flags exactly when they were pushed.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// The environment variable that decides the startup terminal probe:
///
/// * unset — one combined query asks the terminal for the kitty keyboard flags and for its
///   graphics protocol (ADR 0028 R10), with a **2 s** bound;
/// * `0`/`false` — never ask: no keyboard flags are pushed and images draw as half blocks
///   (the pty tests set this: a pty has nobody to answer, and the probe would cost 2 s per
///   spawn);
/// * `1`/`true` — push the keyboard flags without asking, and still probe for graphics (a
///   pty test can then assert the push/pop sequences).
pub const KEYBOARD_ENHANCEMENT_ENV: &str = "RADIAN_KEYBOARD_ENHANCEMENT";

/// How long the one startup probe waits for the terminal's answers (the crossterm keyboard
/// probe's bound, kept for the merged graphics query).
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2000);

/// The picker the transcript draws images with (ADR 0028 R10). Set once by
/// [`TtyGuard::enter`] from the startup probe; `None` in a process that never entered a
/// terminal (the layout tests), where images draw as nothing and the placeholder row is all.
static IMAGE_PICKER: std::sync::OnceLock<Picker> = std::sync::OnceLock::new();

/// The startup probe's picker, or `None` when no terminal was entered.
pub fn image_picker() -> Option<&'static Picker> {
    IMAGE_PICKER.get()
}

/// The startup information line naming the graphics protocol (ADR 0028 R10): what the probe
/// picked and the cell size it measured. `None` before the terminal is entered.
pub fn images_line() -> Option<String> {
    let picker = IMAGE_PICKER.get()?;
    let protocol = match picker.protocol_type() {
        ProtocolType::Halfblocks => "half blocks",
        ProtocolType::Sixel => "sixel",
        ProtocolType::Kitty => "kitty",
        ProtocolType::Iterm2 => "iTerm2",
    };
    let font = picker.font_size();
    Some(format!(
        "images: {protocol} (cell {}×{} px)",
        font.width, font.height
    ))
}

/// What [`KEYBOARD_ENHANCEMENT_ENV`] says: `Some(true)` force on, `Some(false)` off, `None`
/// ask the terminal.
fn keyboard_enhancement_override() -> Option<bool> {
    match std::env::var(KEYBOARD_ENHANCEMENT_ENV).ok().as_deref() {
        Some("0") | Some("false") => Some(false),
        Some("1") | Some("true") => Some(true),
        _ => None,
    }
}

/// Whether the terminal should get the keyboard-enhancement flags, and the picker the
/// transcript draws images with: **one** startup pass (ADR 0028 R10).
///
/// The combined query (`ESC[?u` plus `ratatui-image`'s graphics queries) is written once and
/// its answers are parsed for both: the kitty keyboard flags and the graphics protocol/cell
/// size. It must run **before** the input thread reads: both read the same bytes from stdin.
/// A terminal that answers nothing keeps the keyboard result at "unsupported" and falls back
/// to half blocks. `RADIAN_KEYBOARD_ENHANCEMENT` still skips (`0`) or forces (`1`) the keyboard
/// half; `0` skips the whole pass, because a pty has nobody to answer and would wait the full
/// [`PROBE_TIMEOUT`] per spawn.
fn probe_terminal() -> (bool, Picker) {
    match keyboard_enhancement_override() {
        Some(false) => (false, Picker::halfblocks()),
        Some(true) => {
            let reply = probe_with(&mut StdioProbe);
            (true, picker_from_reply(reply))
        }
        None => {
            let reply = probe_with(&mut StdioProbe);
            (reply.keyboard, picker_from_reply(reply))
        }
    }
}

/// The startup probe's I/O, split from its query and parser so a test can drive one canned
/// answer in (ADR 0028 R10's one-pass rule).
pub(crate) trait ProbeIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn read(&mut self, timeout: Duration) -> io::Result<Vec<u8>>;
}

/// Writes the one combined query and parses the one answer (ADR 0028 R10). A write failure is
/// a no-answer probe, never a startup failure.
pub(crate) fn probe_with(io: &mut impl ProbeIo) -> ProbeReply {
    let query = probe_query(in_tmux());
    if io.write(query.as_bytes()).is_err() {
        return ProbeReply::default();
    }
    let reply = io.read(PROBE_TIMEOUT).unwrap_or_default();
    parse_probe_reply(&reply)
}

/// The single query: `ratatui-image`'s graphics/font-size queries plus the kitty keyboard
/// flags query (`ESC[?u`), inside the tmux passthrough wrapper when needed. `ESC[c` (device
/// attributes) is in the graphics query already, so the keyboard probe's own DA1 is not sent
/// a second time.
pub(crate) fn probe_query(tmux: bool) -> String {
    let mut query = CapParser::query(tmux, QueryStdioOptions::default());
    let (_, escape, end) = CapParser::tmux_start_escape_end(tmux);
    let keyboard = format!("{escape}[?u");
    // Inside the wrapper, before its terminator; at the end otherwise.
    let at = query.len() - end.len();
    query.insert_str(at, &keyboard);
    query
}

/// What one startup probe answered (ADR 0028 R10).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProbeReply {
    /// The kitty keyboard-enhancement flags were answered (`CSI ? <flags> u`).
    pub keyboard: bool,
    /// The graphics protocol the terminal reported, if any.
    pub protocol: Option<ProtocolType>,
    /// The cell size in pixels (`CSI 6 ; height ; width t`).
    pub cell: Option<(u16, u16)>,
}

/// Parses the one reply for both answers: the graphics parser reads kitty/sixel/cell size, and
/// the keyboard flags are the `CSI ? <digits> u` reply crossterm's probe used to wait for.
pub(crate) fn parse_probe_reply(bytes: &[u8]) -> ProbeReply {
    let text = String::from_utf8_lossy(bytes);
    let mut parser = CapParser::new();
    let mut reply = ProbeReply {
        keyboard: keyboard_flags(&text),
        ..ProbeReply::default()
    };
    for ch in text.chars() {
        for response in parser.push(ch) {
            match response {
                Response::Kitty => reply.protocol = Some(ProtocolType::Kitty),
                Response::Sixel => {
                    reply.protocol.get_or_insert(ProtocolType::Sixel);
                }
                Response::CellSize(Some((width, height))) if width > 0 && height > 0 => {
                    reply.cell = Some((width, height));
                }
                _ => {}
            }
        }
    }
    reply
}

/// Whether the reply carries a kitty keyboard-flags answer (`CSI ? <flags> u`). The device
/// attributes answer is `CSI ? … c` and must not count. The `[?` may be reached through a
/// doubled escape (tmux passthrough), so only the byte before it must be an ESC.
fn keyboard_flags(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'[' && bytes[index + 1] == b'?' && index > 0 && bytes[index - 1] == 0x1b
        {
            let mut at = index + 2;
            let mut digit = false;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b';') {
                digit |= bytes[at].is_ascii_digit();
                at += 1;
            }
            if digit && at < bytes.len() && bytes[at] == b'u' {
                return true;
            }
            index = at.max(index + 1);
        } else {
            index += 1;
        }
    }
    false
}

/// The real probe: stdout for the query, stdin for the answer (both raw-mode terminals).
struct StdioProbe;

impl ProbeIo for StdioProbe {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()
    }

    fn read(&mut self, timeout: Duration) -> io::Result<Vec<u8>> {
        read_probe_reply(timeout)
    }
}

/// Reads the terminal's answers until the device-status report (`ESC[0n`, which ends
/// `ratatui-image`'s query) or the timeout. A terminal that answers nothing returns whatever
/// arrived (nothing) at the deadline.
#[cfg(unix)]
fn read_probe_reply(timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = std::time::Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        let mut poll = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd of our own, and the timeout is positive.
        let ready = unsafe { libc::poll(&mut poll, 1, millis.max(1)) };
        if ready <= 0 {
            break;
        }
        // SAFETY: `buf` is writable for its whole length and fd 0 is our stdin.
        let read = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if read <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
        if out.windows(4).any(|window| window == b"\x1b[0n") {
            break;
        }
    }
    Ok(out)
}

/// Windows (and any non-unix target): the probe is not implemented, so the keyboard flags are
/// asked for the way crossterm does and images fall back to half blocks.
#[cfg(not(unix))]
fn read_probe_reply(_timeout: Duration) -> io::Result<Vec<u8>> {
    Ok(Vec::new())
}

/// The picker one reply describes (ADR 0028 R10): the terminal's reported protocol, its cell
/// size, and `ratatui-image`'s own env/tmux fallbacks through its picker. A terminal that
/// reported no cell size uses the window's pixel size when the ioctl reports one; with none,
/// half blocks (the only protocol that does not need a cell size). A terminal that answered
/// nothing falls back to half blocks even when the environment names a protocol: silence is
/// not an answer.
fn picker_from_reply(reply: ProbeReply) -> Picker {
    let Some(font_size) = reply.cell.map(FontSize::from).or_else(window_cell_size) else {
        return Picker::halfblocks();
    };
    // `from_fontsize` is the only constructor that takes a measured cell size; it also runs
    // the tmux detection (the `allow-passthrough` side effect) and the env-based protocol
    // guesses, which the probed answer overrides below.
    #[allow(deprecated)]
    let picker = Picker::from_fontsize(font_size);
    let blacklisted = std::env::var("WEZTERM_EXECUTABLE").is_ok_and(|value| !value.is_empty())
        || std::env::var("KONSOLE_VERSION").is_ok_and(|value| !value.is_empty());
    apply_probe_protocol(picker, reply.protocol, blacklisted)
}

/// Applies one probe answer to the picker `ratatui-image` built from the cell size and the
/// environment (ADR 0028 R10). A reported protocol wins; the two terminals whose kitty/sixel
/// implementations `ratatui-image` refuses (its own query blacklists them) keep the
/// environment's derived protocol; a **silent** probe is half blocks, never an environment
/// guess.
fn apply_probe_protocol(
    mut picker: Picker,
    protocol: Option<ProtocolType>,
    blacklisted: bool,
) -> Picker {
    match (protocol, blacklisted) {
        (Some(protocol), false) => picker.set_protocol_type(protocol),
        (Some(_), true) => {}
        (None, _) => picker.set_protocol_type(ProtocolType::Halfblocks),
    }
    picker
}

/// The cell size from the terminal's pixel/column/row window size, when the ioctl reports one.
fn window_cell_size() -> Option<FontSize> {
    let size = ratatui::crossterm::terminal::window_size().ok()?;
    if size.width == 0 || size.height == 0 || size.columns == 0 || size.rows == 0 {
        return None;
    }
    Some(FontSize::new(
        size.width / size.columns,
        size.height / size.rows,
    ))
}

/// Whether the process runs inside tmux, by the same env rule `ratatui-image` uses. tmux needs
/// the query wrapped in a passthrough sequence, and the picker turns on the pane's
/// `allow-passthrough` option (accepted side effect, ADR 0028 R10).
fn in_tmux() -> bool {
    std::env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
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
        // The startup probe runs **here**, before the caller spawns the input thread (see
        // `probe_terminal`): it reads the terminal's replies, and an input thread reading at
        // the same time would race it. The one pass answers both the kitty keyboard flags
        // (`DISAMBIGUATE_ESCAPE_CODES` makes Shift+Enter arrive as Enter+SHIFT instead of a
        // plain Enter) and the graphics protocol the transcript's images use (ADR 0028 R10).
        let (keyboard, picker) = probe_terminal();
        let _ = IMAGE_PICKER.set(picker);
        if keyboard {
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
        // The diagnostics directory (ADR 0013 T4): owner-only, cleaned before the
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

    /// ADR 0028 R10: one startup pass carries both questions — the kitty keyboard flags
    /// (`ESC[?u`) and `ratatui-image`'s graphics query, whose own `ESC[c` is the only device
    /// attributes query. A second, separate probe would show up as a second write here.
    #[test]
    fn the_startup_probe_is_one_combined_write() {
        struct OneShot {
            writes: Vec<String>,
            reply: Vec<u8>,
        }
        impl ProbeIo for OneShot {
            fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
                self.writes
                    .push(String::from_utf8_lossy(bytes).into_owned());
                Ok(())
            }
            fn read(&mut self, _timeout: Duration) -> io::Result<Vec<u8>> {
                Ok(self.reply.clone())
            }
        }
        let reply = b"\x1b[?1u\x1b[?62;4c\x1b[6;20;10t\x1b[0n".to_vec();
        let mut io = OneShot {
            writes: Vec::new(),
            reply,
        };
        let parsed = probe_with(&mut io);
        assert_eq!(io.writes.len(), 1, "one startup probe pass, not two");
        let query = &io.writes[0];
        assert_eq!(
            query.matches("\x1b[c").count(),
            1,
            "exactly one device-attributes query: {query:?}"
        );
        assert!(
            query.contains("\x1b[?u"),
            "the keyboard flags query: {query:?}"
        );
        assert!(
            query.contains("_Gi=31"),
            "the kitty graphics query: {query:?}"
        );
        assert!(query.contains("\x1b[16t"), "the cell-size query: {query:?}");
        assert!(
            query.contains("\x1b[5n"),
            "the status end marker: {query:?}"
        );
        // Both answers come from that one reply.
        assert!(parsed.keyboard, "the keyboard flags answer");
        assert_eq!(parsed.protocol, Some(ProtocolType::Sixel));
        assert_eq!(parsed.cell, Some((10, 20)));
    }

    /// The graphics answers: kitty wins over sixel; a device-attributes reply alone is not a
    /// keyboard answer.
    #[test]
    fn the_reply_parser_reads_each_answer_from_its_own_sequence() {
        let kitty = parse_probe_reply(b"\x1b_Gi=31;OK\x1b\\\x1b[?64;4c\x1b[0n");
        assert_eq!(kitty.protocol, Some(ProtocolType::Kitty));
        assert!(!kitty.keyboard, "DA1 is not the keyboard flags reply");
        assert_eq!(kitty.cell, None);

        let da_only = parse_probe_reply(b"\x1b[?62c\x1b[0n");
        assert!(!da_only.keyboard, "the DA answer must not count");
        assert_eq!(da_only.protocol, None);

        let flags = parse_probe_reply(b"\x1b[?3u");
        assert!(flags.keyboard);
        assert_eq!(flags.protocol, None);
        // Through tmux's passthrough the escape may arrive doubled.
        assert!(
            parse_probe_reply(b"\x1b\x1b[?1u").keyboard,
            "a doubled escape still carries the flags"
        );
    }

    /// A terminal that answers nothing: the keyboard result stays unsupported (the old
    /// behaviour) and no graphics protocol is picked, which is the half-block fallback even
    /// when the environment (iTerm2, WezTerm) names one.
    #[test]
    fn a_silent_terminal_keeps_unsupported_keyboard_and_no_protocol() {
        assert_eq!(parse_probe_reply(b""), ProbeReply::default());
        assert!(!parse_probe_reply(b"").keyboard);
        assert_eq!(parse_probe_reply(b"").protocol, None);
        assert_eq!(parse_probe_reply(b"garbage without escapes").protocol, None);

        // The picker cannot inherit the environment's guess for a terminal that stayed quiet
        // (ADR 0028 R10).
        #[allow(deprecated)]
        let mut guessed = Picker::from_fontsize(FontSize::new(10, 20));
        guessed.set_protocol_type(ProtocolType::Iterm2);
        let silent = apply_probe_protocol(guessed, None, false);
        assert_eq!(silent.protocol_type(), ProtocolType::Halfblocks);
        assert_eq!(
            (silent.font_size().width, silent.font_size().height),
            (10, 20)
        );
        // A reported protocol still wins, and so does a cell size without one.
        #[allow(deprecated)]
        let mut guessed = Picker::from_fontsize(FontSize::new(10, 20));
        guessed.set_protocol_type(ProtocolType::Iterm2);
        let answered = apply_probe_protocol(guessed, Some(ProtocolType::Sixel), false);
        assert_eq!(answered.protocol_type(), ProtocolType::Sixel);
        let via_reply = picker_from_reply(ProbeReply {
            keyboard: false,
            protocol: None,
            cell: Some((10, 20)),
        });
        assert_eq!(via_reply.protocol_type(), ProtocolType::Halfblocks);
    }

    /// tmux wraps the query for passthrough; the keyboard query goes inside the same wrapper.
    #[test]
    fn the_tmux_query_wraps_everything_in_one_passthrough() {
        let query = probe_query(true);
        assert!(query.starts_with("\x1bPtmux;"), "{query:?}");
        assert!(query.ends_with("\x1b\\"), "{query:?}");
        assert!(query.contains("\x1b\x1b[?u"), "the escaped keyboard query");
        assert_eq!(query.matches("[c").count(), 1);
    }

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
