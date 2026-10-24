//! The arc TUI's scheduling loop (ADR 0048 decision 13).
//!
//! While the TUI is open it is the scheduling loop when it can be. It takes `runner.lock`
//! without waiting: with the lock it runs the pass `radian arc run --watch` runs
//! ([`radian_arc::run::pass`], the same function, not a copy) on a background thread, again
//! [`PASS_INTERVAL`] after a pass that ends quiet and at once after an owner action changed
//! state. Without the lock it is a viewer: the header reads the holder's pid from the lock file,
//! and the next refresh after the holder releases it takes the lock.
//!
//! The UI thread never waits on a pass. A transition line reaches it through a channel, which is
//! what the footer's ticker and the log view draw. On quit (or SIGTERM) the loop does what
//! `--watch` does on SIGTERM: no new work, a short grace for the pass's current step, the
//! running gates stopped the way `--watch` stops them, `runner.lock` released, and a pass that
//! is still running is left to the next loop's reconcile, exactly as when `arc watch` is killed.
//!
//! [`Pass`] is the seam a test uses to run a slow or scripted pass in place of the library's;
//! production code always uses [`LibraryPass`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use radian_arc::run::{self, Report};
use radian_arc::store::{Error, RunnerLock, Store};

/// How long the loop waits after a pass that ended quiet before it runs another.
pub const PASS_INTERVAL: Duration = Duration::from_secs(30);
/// How long a quit waits for the pass thread before it releases `runner.lock` and leaves. A
/// pass's current step (a fetch, a gate start) is not waited for beyond it.
pub const QUIT_GRACE: Duration = Duration::from_secs(2);
/// How often the wait and the quit look at the stop flag and the wake flag.
const POLL: Duration = Duration::from_millis(100);
/// Test-only (an input that exists only for tests is not in the manual): the pass interval in
/// milliseconds, so a test does not wait 30 seconds per pass. Never set by arc.
pub const TEST_INTERVAL_ENV: &str = "RADIAN_ARC_TEST_PASS_INTERVAL_MS";

/// What the loop owns: `runner.lock` in this process, the lock in another process (with its
/// pid when the holder recorded one), or no loop at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStatus {
    /// No process holds `runner.lock`.
    Stopped,
    /// This TUI holds the lock and runs the pass.
    ThisWindow,
    /// Another process holds the lock; the pid is what the holder wrote there, when readable.
    Elsewhere(Option<u32>),
}

/// One thing the pass produced, for the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEvent {
    /// A transition line: the text `radian arc run` prints, without its newline. It goes to the
    /// ticker and to the log.
    Transition(String),
    /// A pass's summary block, or an error line: log only.
    Detail(String),
}

/// One pass a loop can run. [`LibraryPass`] is the real one; a test hands in its own.
pub trait Pass: Send + 'static {
    /// Runs the passes of one loop until one changes nothing. `emit` receives each transition
    /// line with its newline; `stop` ends the loop between passes and between steps.
    ///
    /// # Errors
    ///
    /// The same as [`radian_arc::run::pass`].
    fn run(
        &self,
        store: &Store,
        emit: &mut dyn FnMut(&str),
        stop: &AtomicBool,
    ) -> Result<Report, Error>;
}

/// The real pass: the library function `radian arc run` and `--watch` share.
pub struct LibraryPass;

impl Pass for LibraryPass {
    fn run(
        &self,
        store: &Store,
        emit: &mut dyn FnMut(&str),
        stop: &AtomicBool,
    ) -> Result<Report, Error> {
        run::pass(store, emit, stop)
    }
}

/// The stop flag a SIGTERM sets. A signal handler may only touch an atomic, so the flag is a
/// process-wide static; the loop reads it next to its own stop flag.
static SIGNALLED: AtomicBool = AtomicBool::new(false);

/// Installs the SIGTERM handler of the arc view: the loop stops the way `--watch` stops, the
/// terminal is restored, and the process exits `0`. Ctrl-C is a key in the raw-mode view, not a
/// signal.
#[cfg(unix)]
pub fn install_stop_handler() {
    // SAFETY: `request_stop` has the C ABI `signal` expects, and its body only stores to an
    // `AtomicBool`. The previous handler is not needed.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_stop as *const () as libc::sighandler_t,
        );
    }
}

/// No signal handler on a platform without POSIX signals; the view is quit with `q`.
#[cfg(not(unix))]
pub fn install_stop_handler() {}

#[cfg(unix)]
extern "C" fn request_stop(_signal: libc::c_int) {
    SIGNALLED.store(true, Ordering::SeqCst);
}

/// Whether a SIGTERM arrived.
#[must_use]
pub fn signal_received() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

/// The pass interval: [`PASS_INTERVAL`], or the test-only override.
fn interval() -> Duration {
    std::env::var_os(TEST_INTERVAL_ENV)
        .and_then(|value| value.to_str()?.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(PASS_INTERVAL)
}

/// The scheduling loop of one arc TUI. Dropping it does not stop the thread (that would join);
/// [`ArcLoop::stop`] is the quit path.
pub struct ArcLoop {
    store: Store,
    status: LoopStatus,
    /// The lock this process holds, while it holds one. `stop` drops it.
    lock: Option<RunnerLock>,
    stop: Arc<AtomicBool>,
    wake: Arc<AtomicBool>,
    events: Receiver<LoopEvent>,
    handle: Option<JoinHandle<()>>,
    /// The pass to run when the lock is taken. Used once: a loop is stopped for good.
    pass: Option<Box<dyn Pass>>,
    interval: Duration,
    /// [`ArcLoop::stop`] ran: a stopped loop does not take the lock again.
    stopped: bool,
}

impl ArcLoop {
    /// Opens the loop on `store`: it takes `runner.lock` when it can, and is a viewer
    /// otherwise. [`ArcLoop::refresh`] tries again.
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self::with_pass(store, Box::new(LibraryPass))
    }

    /// [`ArcLoop::new`] with the pass a test hands in (a slow or scripted one). Production code
    /// uses [`ArcLoop::new`].
    #[must_use]
    pub fn with_pass(store: Store, pass: Box<dyn Pass>) -> Self {
        Self::with_pass_and_interval(store, pass, interval())
    }

    /// [`ArcLoop::with_pass`] with an explicit pass interval: a test does not set the test-only
    /// environment for the whole process.
    #[must_use]
    pub fn with_pass_and_interval(store: Store, pass: Box<dyn Pass>, interval: Duration) -> Self {
        let (_sender, events) = mpsc::channel();
        let mut arc_loop = Self {
            store,
            status: LoopStatus::Stopped,
            lock: None,
            stop: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(AtomicBool::new(false)),
            events,
            handle: None,
            pass: Some(pass),
            interval,
            stopped: false,
        };
        arc_loop.take();
        arc_loop
    }

    /// The loop's state for the header.
    #[must_use]
    pub fn status(&self) -> LoopStatus {
        self.status
    }

    /// Whether this process holds `runner.lock`.
    #[must_use]
    pub fn holds_lock(&self) -> bool {
        self.lock.is_some()
    }

    /// Tries the lock again when this TUI does not hold it: a viewer becomes the loop on the
    /// refresh after the other holder exits.
    pub fn refresh(&mut self) {
        if self.stopped {
            return;
        }
        if self.lock.is_none() && self.handle.is_none() {
            self.take();
        }
    }

    /// Runs the pass at once instead of at the end of the interval. An owner action that
    /// changed state calls this; it does nothing while the TUI is a viewer.
    pub fn nudge(&mut self) {
        self.wake.store(true, Ordering::SeqCst);
    }

    /// Every event the pass produced since the last call.
    pub fn drain(&mut self) -> Vec<LoopEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            events.push(event);
        }
        events
    }

    /// The quit path: no new work, a short grace for the pass's current step, `runner.lock`
    /// released. A pass still running is left to the next loop's reconcile, exactly as when
    /// `arc run --watch` is killed (ADR 0048 decision 13).
    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.stop.store(true, Ordering::SeqCst);
            self.wake.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + QUIT_GRACE;
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(POLL);
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        // Releasing the lock is this side's job, not the thread's: a pass that is still stuck
        // must not keep another loop out after this process is gone.
        self.lock = None;
        self.status = LoopStatus::Stopped;
        self.stopped = true;
    }

    /// Tries `runner.lock`; with it, starts the pass thread.
    fn take(&mut self) {
        if self.stopped {
            return;
        }
        match self.store.try_runner_lock() {
            Ok(Some(lock)) => {
                self.lock = Some(lock);
                self.status = LoopStatus::ThisWindow;
                self.stop = Arc::new(AtomicBool::new(false));
                self.wake = Arc::new(AtomicBool::new(false));
                let (sender, events) = mpsc::channel();
                self.events = events;
                let Some(pass) = self.pass.take() else {
                    // A loop that was already stopped: no second thread, no pass.
                    self.lock = None;
                    self.status = LoopStatus::Stopped;
                    return;
                };
                let store = self.store.clone();
                let stop = Arc::clone(&self.stop);
                let wake = Arc::clone(&self.wake);
                let interval = self.interval;
                self.handle = Some(std::thread::spawn(move || {
                    run_loop(store, pass, stop, wake, interval, sender);
                }));
            }
            // Another loop holds it: the pid the holder wrote names it for the header.
            Ok(None) => self.status = LoopStatus::Elsewhere(self.store.runner_lock_pid()),
            Err(_) => self.status = LoopStatus::Stopped,
        }
    }
}

/// The background thread: pass, wait, pass, until the stop flag is set (by [`ArcLoop::stop`]
/// or by SIGTERM). Then stop the gates the way `--watch` does and return.
fn run_loop(
    store: Store,
    pass: Box<dyn Pass>,
    stop: Arc<AtomicBool>,
    wake: Arc<AtomicBool>,
    interval: Duration,
    sender: Sender<LoopEvent>,
) {
    let stopped = || stop.load(Ordering::SeqCst) || signal_received();
    loop {
        if stopped() {
            break;
        }
        let mut emit = |line: &str| {
            for text in line.lines() {
                if !text.is_empty() {
                    let _ = sender.send(LoopEvent::Transition(text.to_owned()));
                }
            }
        };
        match pass.run(&store, &mut emit, &stop) {
            Ok(report) => {
                let _ = sender.send(LoopEvent::Detail(report.render_summary()));
            }
            // The UI shows the line in its log and goes on: `--watch` prints it and runs the
            // next pass, and the lock and every state file stay as they are.
            Err(error) => {
                let _ = sender.send(LoopEvent::Transition(format!(
                    "radian arc: {}",
                    error.message()
                )));
            }
        }
        if !wait(&stop, &wake, interval) {
            break;
        }
    }
    if stopped() {
        let _ = run::stop_gates(&store);
    }
}

/// Waits `interval`, waking early on a nudge. `false` when a stop was requested first.
fn wait(stop: &AtomicBool, wake: &AtomicBool, interval: Duration) -> bool {
    let deadline = Instant::now() + interval;
    loop {
        if stop.load(Ordering::SeqCst) || signal_received() {
            return false;
        }
        if wake.swap(false, Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep(POLL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stopped_loop_has_no_status_text_and_the_others_name_the_lock() {
        assert_eq!(LoopStatus::Stopped, LoopStatus::Stopped);
        assert_ne!(LoopStatus::ThisWindow, LoopStatus::Elsewhere(None));
        assert_ne!(
            LoopStatus::Elsewhere(Some(1)),
            LoopStatus::Elsewhere(Some(2))
        );
    }
}
