//! The process one gate stage runs, and the stop that reaches it (ADR 0048 decisions 10 and
//! 13).
//!
//! A stage's `sh -c` command and the `review` session each run in a process group of their
//! own. When `serve` stops it signals the gate, not those children; before this module the
//! child kept running and could write in the stage's checkout after the gate (and its locks)
//! were gone. The gate installs a SIGTERM handler here, every running stage registers the child
//! it runs, and the handler forwards the stop to each child's whole group: SIGTERM at once,
//! SIGKILL after `[limits] cancel_grace_secs` from the stage's wait loop. A `commands`/`review`
//! pair (ADR 0048 decision 10) runs two children, so the stop must reach both. The gate then
//! exits without a record, so the next loop starts the stage again as unverified.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::process;

/// How many stage children one gate can track at once. A gate runs at most the parallel
/// `commands`/`review` pair (ADR 0048 decision 10), and a process that runs stages directly
/// (a test, a synchronous verification) may run several verifications in threads; the slack
/// keeps a slot free in every ordinary case.
const CHILD_SLOTS: usize = 8;
/// The stage children that run now, one pid per slot (zero is a free slot). A signal handler
/// can only touch atomics, so the pids live in a fixed array rather than a collection: a stop
/// reaches every stage it finds (ADR 0048 decision 10).
static CHILD_PIDS: [AtomicI32; CHILD_SLOTS] = [const { AtomicI32::new(0) }; CHILD_SLOTS];
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
    for slot in &CHILD_PIDS {
        let pid = slot.load(Ordering::SeqCst);
        if pid > 0 {
            process::signal_group(pid as u32, process::SIGTERM);
        }
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
    /// The slot the pid occupies in [`CHILD_PIDS`], when one was free.
    slot: Option<usize>,
    signalled: Option<Instant>,
}

impl Running {
    /// Starts tracking the stage child `pid`. The gate's SIGTERM handler forwards to it too.
    /// A child spawned after the stop signal never saw that handler, so it is signalled here.
    pub(crate) fn track(pid: u32) -> Self {
        let slot = CHILD_PIDS.iter().position(|slot| {
            slot.compare_exchange(0, pid as i32, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        });
        if stop_requested() {
            process::signal_group(pid, process::SIGTERM);
        }
        Self {
            pid,
            slot,
            signalled: None,
        }
    }

    /// Called from the stage's wait loop. The handler sent SIGTERM when the stop was requested;
    /// once a child ignores it for the grace, this sends SIGKILL to its group. The loop keeps
    /// waiting and treats the exit as a normal result.
    pub(crate) fn stopped(&mut self) {
        if !stop_requested() {
            return;
        }
        match self.signalled {
            None => {
                self.signalled = Some(Instant::now());
            }
            Some(at) => {
                let grace = Duration::from_millis(GRACE_MILLIS.load(Ordering::SeqCst));
                if Instant::now() >= at + grace {
                    process::signal_group(self.pid, process::SIGKILL);
                }
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            CHILD_PIDS[slot].store(0, Ordering::SeqCst);
        }
    }
}
