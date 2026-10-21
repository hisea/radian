//! Spawning and supervising one server process.
//!
//! The server runs in its own process group (Unix), so killing it also kills anything it
//! started. A dedicated OS thread owns the `Child`, so reaping needs no Tokio runtime. As soon
//! as the leader exits, for any reason, that thread kills what is left of its process group,
//! so a server that crashes cannot leave helpers behind. Killing is a synchronous signal, so it
//! also works from `Drop` while a runtime is shutting down.
//!
//! A process group id is the leader's pid, and it can be reused once the leader is reaped and
//! the group is empty. So the group is only ever signalled while the leader is still
//! unreaped: the reaper first waits for the exit *without* reaping (`waitid` with `WNOWAIT`),
//! kills the group while the leader is a zombie (a zombie still holds its pid and group), and
//! only then reaps. Every signal and the reap itself take the same lock, so no other path can
//! signal the group between the reap and the end of the check.

use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;

use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::watch;

use crate::config::McpServerConfig;

pub(crate) struct ServerProcess {
    pid: u32,
    exited: watch::Receiver<bool>,
    /// `true` once the leader has been reaped. The group id may belong to someone else from
    /// then on, so nothing may signal it. Held across every signal and across the reap.
    reaped: Arc<Mutex<bool>>,
}

impl ServerProcess {
    /// Must be called inside a Tokio runtime (the pipes register with its reactor).
    pub(crate) fn spawn(config: &McpServerConfig) -> io::Result<(Self, ChildStdout, ChildStdin)> {
        Self::spawn_with(config, start_reaper_thread)
    }

    /// `start_reaper` runs the reaper body on a new thread. It is a parameter so that a test
    /// can make thread creation fail.
    ///
    /// The reaper thread is created before the child. A thread the OS refuses to create then
    /// costs nothing: no child exists yet. Once the child runs, it is handed to the reaper
    /// through a channel, and every failure from there on is cleaned up by this thread, which
    /// still owns the child until the hand-over succeeds.
    fn spawn_with(
        config: &McpServerConfig,
        start_reaper: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
    ) -> io::Result<(Self, ChildStdout, ChildStdin)> {
        let (child_tx, child_rx) = mpsc::channel::<Child>();
        let (exited_tx, exited) = watch::channel(false);
        let reaped = Arc::new(Mutex::new(false));
        let reaped_by_reaper = Arc::clone(&reaped);
        start_reaper(Box::new(move || {
            // The sender is dropped without sending when the child could not be started.
            let Ok(mut child) = child_rx.recv() else {
                return;
            };
            let pid = child.id();
            wait_for_exit_without_reaping(pid);
            {
                let mut reaped = lock(&reaped_by_reaper);
                // The leader is a zombie or already reaped by nobody else, so its pid and
                // group are still reserved and this reaches only the leftover members.
                kill_group(pid);
                let _ = child.wait();
                *reaped = true;
            }
            let _ = exited_tx.send(true);
        }))?;

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        for key in &config.inherit_env {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.envs(config.env.iter().map(|(k, v)| (k, v)));
        // ADR 0043 decision 2 (fix round 1 A): a subagent child gets its parent's `--header`
        // values in `RADIAN_CHILD_HEADERS` and removes the variable from its own environment at
        // startup, so this is belt and braces — a server must not see that secret even if a
        // config names the variable in `inherit_env`. The name is `radian_host::headers::
        // CHILD_HEADERS_ENV`; `agent-extensions` cannot depend on `radian-host` (the dependency
        // runs the other way), so it is written out here.
        command.env_remove("RADIAN_CHILD_HEADERS");
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command.spawn()?;
        let pid = child.id();
        let pipes = child
            .stdin
            .take()
            .zip(child.stdout.take())
            .ok_or_else(|| io::Error::other("server stdio was not piped"))
            .and_then(|(stdin, stdout)| {
                Ok((ChildStdout::from_std(stdout)?, ChildStdin::from_std(stdin)?))
            });
        let (stdout, stdin) = match pipes {
            Ok(pipes) => pipes,
            Err(e) => {
                stop_unreaped(child);
                return Err(e);
            }
        };
        if let Err(mpsc::SendError(child)) = child_tx.send(child) {
            // The reaper thread is gone, so nothing else will ever reap this child.
            stop_unreaped(child);
            return Err(io::Error::other(format!(
                "the reaper thread ended early (server process {pid} was stopped)"
            )));
        }
        Ok((
            Self {
                pid,
                exited,
                reaped,
            },
            stdout,
            stdin,
        ))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Kills the whole process group. Synchronous and safe to call repeatedly. Does nothing
    /// once the leader has been reaped, because the group id may belong to someone else by
    /// then. The lock makes "not reaped yet" hold until the signal is sent.
    pub(crate) fn kill_tree(&self) {
        let reaped = lock(&self.reaped);
        if !*reaped {
            kill_group(self.pid);
        }
    }

    /// Waits until the server process has exited and been reaped, up to `limit`.
    pub(crate) async fn wait_exit(&self, limit: Duration) -> bool {
        let mut exited = self.exited.clone();
        tokio::time::timeout(limit, exited.wait_for(|done| *done))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// Kills the tree and waits for the leader to be reaped.
    pub(crate) async fn kill_and_reap(&self, limit: Duration) -> bool {
        self.kill_tree();
        self.wait_exit(limit).await
    }
}

fn start_reaper_thread(body: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    thread::Builder::new()
        .name("mcp-reaper".into())
        .spawn(body)
        .map(|_| ())
}

/// Kills a child that no reaper owns and reaps it here.
fn stop_unreaped(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn lock(mutex: &Mutex<bool>) -> MutexGuard<'_, bool> {
    // The flag is a plain bool, so a poisoned lock still holds a usable value.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Blocks until process `pid` has exited, leaving it unreaped (a zombie) so that its pid and
/// process group stay reserved.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_exit_without_reaping(pid: u32) {
    let id: libc::id_t = pid;
    loop {
        // SAFETY: `info` is zeroed plain data that waitid fills in; no other preconditions.
        let status = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT)
        };
        if status == 0 {
            return;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // Not our child any more, or waitid is unsupported: fall back to reaping directly.
            return;
        }
    }
}

/// Platforms without `waitid(WNOWAIT)` reap directly, so there is a short window between the
/// reap and the group kill. It only matters if the leader was the group's last member.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn wait_for_exit_without_reaping(_pid: u32) {}

/// Sends SIGKILL to process group `pid` (Unix), or terminates the tree rooted at `pid`
/// (Windows).
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(pid) {
            // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        // Interim, and weaker than the Unix path: `taskkill /T` follows parent links, so it
        // cannot reach the children of a leader that has already exited. A Job Object is
        // planned.
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // The reaper thread keeps running and reaps the process after the kill.
        self.kill_tree();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(command: &str) -> McpServerConfig {
        McpServerConfig::new("test", command)
    }

    #[tokio::test]
    async fn no_child_is_started_when_the_reaper_thread_cannot_be_created() {
        // The command does not exist: had the child been spawned first, the error would be
        // NotFound. Getting the thread error instead shows the thread came first.
        let result = ServerProcess::spawn_with(&config("/nonexistent/never-run"), |_body| {
            Err(io::Error::other("no threads left"))
        });

        let error = result.err().expect("spawn must fail");
        assert_eq!(error.to_string(), "no threads left");
    }

    #[tokio::test]
    async fn a_child_is_stopped_and_reaped_when_the_reaper_never_runs() {
        // The thread "starts" but drops its body, so the hand-over of the child fails.
        let result = ServerProcess::spawn_with(&config("/bin/sleep").arg("30"), |body| {
            drop(body);
            Ok(())
        });

        let error = result.err().expect("spawn must fail");
        let message = error.to_string();
        let pid: i32 = message
            .split("server process ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("no pid in {message:?}"));
        // kill(pid, 0) succeeds for a zombie, so failure means the child was reaped too.
        // SAFETY: signal 0 only checks that the process exists.
        let exists = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!exists, "server process {pid} was left behind");
    }
}
