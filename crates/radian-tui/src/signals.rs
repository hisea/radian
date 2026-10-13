//! SIGTERM and SIGHUP end the app the same way a quit does (dogfooding D7).
//!
//! Without this, a SIGTERM killed the process mid-frame: the alternate screen and raw mode
//! stayed on and nothing said why the app was gone (the shell printed `zsh: terminated`). The
//! listener records which signal arrived and hands it to the caller's `notify` (the TUI sends
//! it as a `UiEvent`, ADR 0023 U2: nothing is polled); the loop then takes the ordinary exit
//! path, so the terminal is restored, the driver is stopped, and `main` prints one reason line
//! and exits with `128 + signal`, the shell convention.

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// POSIX fixes these numbers on every Unix this crate runs on (macOS and Linux), so they are
/// named here rather than pulling in `libc` for two constants.
const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;

/// How long the signal task waits for the main loop to confirm it is leaving. The loop is
/// normally blocked on its event channel and wakes immediately; but it can be stuck in a long
/// frame or an effect, or the terminal can be gone. Three seconds is far longer than a wake-up
/// and short enough that a stuck app does not linger.
const EXIT_WATCHDOG: Duration = Duration::from_secs(3);

/// The signal that asked the app to stop, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

/// Notified by the main loop when it is leaving (`confirm_exit`), so the signal task can tell
/// "the loop saw the flag and is cleaning up" from "the loop is stuck".
static EXIT_NOTIFY: std::sync::OnceLock<Notify> = std::sync::OnceLock::new();

fn exit_notify() -> &'static Notify {
    EXIT_NOTIFY.get_or_init(Notify::new)
}

/// The main loop calls this as it leaves, so a pending signal does not trigger the forced exit.
pub fn confirm_exit() {
    exit_notify().notify_one();
}

/// The signal that asked the app to stop, if any.
pub fn received() -> Option<i32> {
    match RECEIVED.load(Ordering::SeqCst) {
        0 => None,
        signal => Some(signal),
    }
}

/// The signal's name for the exit line.
pub fn name(signal: i32) -> &'static str {
    match signal {
        SIGTERM => "SIGTERM",
        SIGHUP => "SIGHUP",
        _ => "a signal",
    }
}

/// Starts listening for SIGTERM and SIGHUP on `runtime`, calling `notify(signal)` once. Installing
/// the handlers replaces the default "terminate now" action, so from here on these signals only
/// set the flag and call the notifier.
#[cfg(unix)]
pub fn listen(
    runtime: &tokio::runtime::Handle,
    notify: impl Fn(i32) + Send + 'static,
) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let _enter = runtime.enter();
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    runtime.spawn(async move {
        let signal = tokio::select! {
            _ = term.recv() => SIGTERM,
            _ = hup.recv() => SIGHUP,
        };
        // The first signal wins; a second one changes nothing.
        let first = RECEIVED
            .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if first {
            // Hand it to the UI as an event (ADR 0023 U2). A second signal is not forwarded:
            // the first one is already taking the app down.
            notify(signal);
        }

        // The loop normally wakes from its event channel immediately. If it does not — it is
        // stuck in a long frame or effect — three seconds pass and this task forces the exit.
        // Restore the terminal first: it may still be alive and showing the alternate screen,
        // and mouse reporting (on by default) would keep sending `ESC[<…M` into the user's
        // shell, so a wheel or click would echo garbage. The restore is best-effort and
        // non-blocking: the stuck loop may hold the output buffer full, and a failed write is
        // ignored — an already-dead terminal (EIO) needs nothing. The main loop may still be
        // writing, so bytes can interleave; each escape sequence is one short write and the
        // process is leaving, so the mouse-off is delivered and a torn frame is cosmetic.
        // Every journal append is already fsynced, so an exit mid-turn recovers by the ADR
        // 0019 / ADR 0002 rules (a torn tail is truncated; an interrupted run leaves memory
        // and journal consistent).
        if tokio::time::timeout(EXIT_WATCHDOG, exit_notify().notified())
            .await
            .is_err()
        {
            crate::terminal::restore_terminal_nonblocking();
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub fn listen(
    _runtime: &tokio::runtime::Handle,
    _notify: impl Fn(i32) + Send + 'static,
) -> std::io::Result<()> {
    Ok(())
}
