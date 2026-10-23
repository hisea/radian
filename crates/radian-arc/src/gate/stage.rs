//! The process one gate stage runs, and the stop that reaches it (ADR 0048 decisions 10 and
//! 13).
//!
//! A stage's `sh -c` command and the `review` session each run in a process group of their
//! own. When `--watch` stops it signals the gate, not those children; before this module the
//! child kept running and could write in the stage's checkout after the gate (and its locks)
//! were gone. The gate installs a SIGTERM handler here, the stage tracks the child it runs,
//! and the wait loop forwards the stop to the child's whole group: SIGTERM at once, SIGKILL
//! after `[limits] cancel_grace_secs`. The gate then exits without a record, so the next loop
//! starts the stage again as unverified.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::process;

/// The stage child's pid, while one runs.
static CHILD_PID: AtomicI32 = AtomicI32::new(0);
/// Whether a SIGTERM asked the gate to stop.
static STOP: AtomicBool = AtomicBool::new(false);
/// The grace between SIGTERM and SIGKILL, in milliseconds. `[limits] cancel_grace_secs`
/// replaces the default once the gate has read `config.toml`.
static GRACE_MILLIS: AtomicU64 = AtomicU64::new(10_000);

/// Installs the gate's stop handler for SIGTERM and SIGINT. The handler only stores to
/// atomics and forwards to the tracked group, which is async-signal-safe. A Ctrl-C sent to the
/// gate alone therefore stops its stage child exactly as SIGTERM does.
pub(crate) fn install_handler() {
    // SAFETY: `stop` has the C ABI `signal` expects, and its body only stores to atomics and
    // calls `killpg(2)`; both are async-signal-safe. The previous handler is not needed.
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT] {
            libc::signal(signal, request_stop as *const () as libc::sighandler_t);
        }
    }
}

extern "C" fn request_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        process::signal_group(pid as u32, process::SIGTERM);
    }
}

/// Whether a stop was requested.
pub(crate) fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}

/// Sets the grace between SIGTERM and SIGKILL for this gate.
pub(crate) fn set_grace(grace: Duration) {
    GRACE_MILLIS.store(
        u64::try_from(grace.as_millis()).unwrap_or(u64::MAX),
        Ordering::SeqCst,
    );
}

/// One stage child, tracked so a stop reaches its process group. Dropping it stops tracking.
pub(crate) struct Running {
    pid: u32,
    signalled: Option<Instant>,
}

impl Running {
    /// Starts tracking the stage child `pid`. The gate's SIGTERM handler forwards to it too.
    pub(crate) fn track(pid: u32) -> Self {
        CHILD_PID.store(pid as i32, Ordering::SeqCst);
        Self {
            pid,
            signalled: None,
        }
    }

    /// Called from the stage's wait loop. Once a stop is requested it sends SIGTERM to the
    /// child's group, then SIGKILL after the grace; `true` when the child has been killed and
    /// the loop should stop treating its exit as a normal result.
    pub(crate) fn stopped(&mut self) -> bool {
        if !stop_requested() {
            return false;
        }
        match self.signalled {
            None => {
                process::signal_group(self.pid, process::SIGTERM);
                self.signalled = Some(Instant::now());
                false
            }
            Some(at) => {
                let grace = Duration::from_millis(GRACE_MILLIS.load(Ordering::SeqCst));
                if Instant::now() >= at + grace {
                    process::signal_group(self.pid, process::SIGKILL);
                    true
                } else {
                    false
                }
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        CHILD_PID.store(0, Ordering::SeqCst);
    }
}
