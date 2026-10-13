//! SIGTERM and SIGHUP end the app the same way a quit does (dogfooding D7).
//!
//! Without this, a SIGTERM killed the process mid-frame: the alternate screen and raw mode
//! stayed on and nothing said why the app was gone (the shell printed `zsh: terminated`). The
//! listener only records which signal arrived; the UI loop sees it on its next tick and takes
//! the ordinary exit path, so the terminal is restored, the driver is stopped, and `main`
//! prints one reason line and exits with `128 + signal`, the shell convention.

use std::sync::atomic::{AtomicI32, Ordering};

/// POSIX fixes these numbers on every Unix this crate runs on (macOS and Linux), so they are
/// named here rather than pulling in `libc` for two constants.
const SIGHUP: i32 = 1;
const SIGTERM: i32 = 15;

/// The signal that asked the app to stop, or 0.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

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
    });
    Ok(())
}

#[cfg(not(unix))]
pub fn listen(_runtime: &tokio::runtime::Handle) -> std::io::Result<()> {
    Ok(())
}
