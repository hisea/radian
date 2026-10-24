//! Starting the arc scheduling loop from the TUI (ADR 0048 decision 13).
//!
//! The TUI never takes `runner.lock` and never runs a pass. It probes the lock (take and
//! release at once, never creating it); when no loop holds it, it starts `radian arc serve`
//! as a detached process in the repository root: its own session (`setsid`, so the
//! terminal's SIGHUP and the TUI's process group never reach it), stdin, stdout and stderr on
//! `/dev/null`, and no handle the TUI reaps or signals when it quits. `R` starts one the same
//! way when the probe says no loop runs.
//!
//! The header reads the probe: `running (pid <n>)` while any loop holds the lock, `starting`
//! from a start until a probe sees the lock, and `stopped` when none holds it. A start that
//! fails (the spawn fails, or the lock is still free [`START_TIMEOUT`] later) is reported once
//! in the footer with the process's exit code when it exited, and the last `radian arc:` line
//! of `loop.log` when there is one.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use radian_arc::store::{RUNNER_LOCK, Store};

use super::loop_log;

/// How long a start has to show its lock before it counts as failed.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The arguments the TUI starts: the scheduling loop with the default interval.
pub const LOOP_ARGS: [&str; 2] = ["arc", "serve"];

/// What the header shows about the scheduling loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopStatus {
    /// No process holds `runner.lock`.
    Stopped,
    /// The TUI started a loop and no probe has seen its lock yet.
    Starting,
    /// A loop holds the lock; the pid is what the holder wrote there, when readable.
    Elsewhere(Option<u32>),
}

/// Starts and watches the detached `radian arc serve` of one repository. It holds no
/// lock itself and never signals the child.
pub struct LoopStarter {
    store: Store,
    status: LoopStatus,
    /// The process this starter spawned, until a probe sees its lock or the start fails. Drop
    /// neither waits for it nor signals it, and the process lives in its own session.
    child: Option<Child>,
    started_at: Option<Instant>,
    timeout: Duration,
    /// Test-only seam (it exists only for tests): the program to start in place of the current
    /// executable. Production always leaves this `None` and starts `std::env::current_exe()`.
    program: Option<PathBuf>,
}

impl LoopStarter {
    /// Probes `runner.lock` (never creating it) and records the header state; it starts
    /// nothing. [`LoopStarter::start`] is what the TUI calls when it opens, and `R` calls.
    #[must_use]
    pub fn new(store: Store) -> Self {
        let status = probe(&store);
        Self {
            store,
            status,
            child: None,
            started_at: None,
            timeout: START_TIMEOUT,
            program: None,
        }
    }

    /// [`LoopStarter::new`] with the program a test starts instead of the current executable.
    /// Test-only: nothing in the binary calls it.
    #[must_use]
    pub fn with_program(store: Store, program: PathBuf) -> Self {
        let mut starter = Self::new(store);
        starter.program = Some(program);
        starter
    }

    /// [`LoopStarter::with_program`] with a shorter start timeout, so a test does not wait
    /// [`START_TIMEOUT`] for a start that fails. Test-only.
    #[must_use]
    pub fn with_program_and_timeout(store: Store, program: PathBuf, timeout: Duration) -> Self {
        let mut starter = Self::with_program(store, program);
        starter.timeout = timeout;
        starter
    }

    /// The header's loop state.
    #[must_use]
    pub fn status(&self) -> LoopStatus {
        self.status
    }

    /// Starts `radian arc serve` when the probe says no loop runs. `Err` is the footer
    /// line: why nothing started (a loop already holds the lock, or the spawn failed).
    ///
    /// # Errors
    ///
    /// A loop already holds `runner.lock`, or the process could not be spawned.
    pub fn start(&mut self) -> Result<(), String> {
        if self.status == LoopStatus::Starting {
            return Err("a loop is starting".to_owned());
        }
        let status = probe(&self.store);
        if status != LoopStatus::Stopped {
            self.status = status;
            return Err(match status {
                LoopStatus::Elsewhere(Some(pid)) => {
                    format!("a loop is already running (pid {pid})")
                }
                _ => "a loop is already running".to_owned(),
            });
        }
        match self.spawn() {
            Ok(child) => {
                self.child = Some(child);
                self.status = LoopStatus::Starting;
                self.started_at = Some(Instant::now());
                Ok(())
            }
            Err(error) => {
                self.status = LoopStatus::Stopped;
                Err(format!("cannot start the loop: {error}"))
            }
        }
    }

    /// Probes the lock again, and a start in flight. Returns the footer line once when the
    /// start failed: the process's exit code when it exited, and the last `radian arc:` line of
    /// `loop.log` when there is one.
    pub fn poll(&mut self) -> Option<String> {
        if self.status == LoopStatus::Starting {
            return self.poll_start();
        }
        let probed = probe(&self.store);
        if probed != self.status {
            self.status = probed;
        }
        None
    }

    /// Polls a start in flight without taking `runner.lock`: a probe takes the lock for an
    /// instant, and a loop that is still starting sees it held and exits `4`. The child's own
    /// pid in the file (written after it locks) is the signal that it took the lock; an exited
    /// child is the failure, explained by its exit code and the last `radian arc:` line.
    fn poll_start(&mut self) -> Option<String> {
        let Some(child) = self.child.as_mut() else {
            self.status = LoopStatus::Stopped;
            return None;
        };
        if let Ok(Some(status)) = child.try_wait() {
            let reason = match status.code() {
                Some(code) => format!("the loop exited {code}"),
                None => "the loop exited after a signal".to_owned(),
            };
            self.child = None;
            self.started_at = None;
            self.status = LoopStatus::Stopped;
            return Some(match loop_log::last_error_line(&self.store) {
                Some(line) => format!("{reason}: {line}"),
                None => reason,
            });
        }
        let pid = self.child.as_ref().map(Child::id);
        if let Some(pid) = pid
            && self.store.runner_lock_pid() == Some(pid)
        {
            // The child is the loop now; the header names it from the lock. Dropping the
            // handle neither waits for it nor signals it.
            self.child = None;
            self.started_at = None;
            self.status = LoopStatus::Elsewhere(Some(pid));
            return None;
        }
        if self.started_at.is_none_or(|at| at.elapsed() < self.timeout) {
            return None;
        }
        let reason = match pid {
            Some(pid) => format!("the loop ({pid}) is running but holds no runner.lock"),
            None => "the loop is running but holds no runner.lock".to_owned(),
        };
        self.child = None;
        self.started_at = None;
        self.status = LoopStatus::Stopped;
        Some(match loop_log::last_error_line(&self.store) {
            Some(line) => format!("{reason}: {line}"),
            None => reason,
        })
    }

    fn spawn(&self) -> std::io::Result<Child> {
        let program = match &self.program {
            Some(program) => program.clone(),
            None => std::env::current_exe()?,
        };
        let mut command = Command::new(program);
        command
            .args(LOOP_ARGS)
            .current_dir(self.store.repo())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // A session of its own: the terminal's SIGHUP and the TUI's process group never reach
        // the loop, and the TUI can exit without the loop seeing anything.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            // SAFETY: `setsid` is async-signal-safe and touches no memory of this process.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        command.spawn()
    }
}

/// Whether any process holds `runner.lock`, and its pid when the holder wrote one. The lock is
/// taken and released at once; the probe never creates the file.
#[must_use]
pub fn probe(store: &Store) -> LoopStatus {
    let path = store.root().join(RUNNER_LOCK);
    if !path.is_file() {
        return LoopStatus::Stopped;
    }
    match store.try_runner_lock() {
        Ok(Some(guard)) => {
            drop(guard);
            LoopStatus::Stopped
        }
        Ok(None) => LoopStatus::Elsewhere(store.runner_lock_pid()),
        Err(_) => LoopStatus::Stopped,
    }
}

/// The stop flag a SIGTERM sets. A signal handler may only touch an atomic, so the flag is a
/// process-wide static; the TUI reads it and closes its terminal. It reaches no loop: a loop
/// the TUI started lives in its own session (ADR 0048 decision 13).
static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Installs the SIGTERM handler of the arc view: the terminal is restored and the process
/// exits `0`. Ctrl-C is a key in the raw-mode view, not a signal.
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
    SIGNALLED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Whether a SIGTERM arrived.
#[must_use]
pub fn signal_received() -> bool {
    SIGNALLED.load(std::sync::atomic::Ordering::SeqCst)
}
