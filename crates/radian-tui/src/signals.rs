//! SIGTERM and SIGHUP end the app the same way a quit does (dogfooding D7).
//!
//! Without this, a SIGTERM killed the process mid-frame: the alternate screen and raw mode
//! stayed on and nothing said why the app was gone (the shell printed `zsh: terminated`). The
//! listener only records which signal arrived; the UI loop sees it on its next tick and takes
//! the ordinary exit path, so the terminal is restored, the driver is stopped, and `main`
//! prints one reason line and exits with `128 + signal`, the shell convention.

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// POSIX fixes these numbers on every Unix this crate runs on (macOS and Linux), so they are
/// named here rather than pulling in `libc` for two constants.
const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;

/// How long the signal task waits for the main loop to confirm it is leaving. The loop is
/// normally in its 50 ms input poll and sees the flag on the next tick; but crossterm's reader
/// can spin forever on a tty that has gone away (see `read_input` in `main.rs`), and then the
/// loop never reaches the flag. Three seconds is far longer than one tick and short enough that
/// a hung app does not linger.
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

/// Starts listening for SIGTERM and SIGHUP on `runtime`. Installing the handlers replaces the
/// default "terminate now" action, so from here on these signals only set the flag.
#[cfg(unix)]
pub fn listen(runtime: &tokio::runtime::Handle) -> std::io::Result<()> {
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
        let _ = RECEIVED.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);

        // The loop normally notices the flag within one 50 ms poll. If it does not — it is
        // inside crossterm's reader, which loops forever when the tty returns EOF/EIO — the
        // terminal is already gone, so restoring it is pointless. Exit with the shell
        // convention. Every journal append is already fsynced, so an exit mid-turn recovers by
        // the ADR 0019 / ADR 0002 rules (a torn tail is truncated; an interrupted run leaves
        // memory and journal consistent).
        if tokio::time::timeout(EXIT_WATCHDOG, exit_notify().notified())
            .await
            .is_err()
        {
            std::process::exit(128 + signal);
        }
    });
    Ok(())
}

#[cfg(not(unix))]
pub fn listen(_runtime: &tokio::runtime::Handle) -> std::io::Result<()> {
    Ok(())
}
