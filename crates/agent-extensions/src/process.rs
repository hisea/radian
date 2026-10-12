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
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
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
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };

        let (exited_tx, exited) = watch::channel(false);
        let reaped = Arc::new(Mutex::new(false));
        let reaped_by_reaper = Arc::clone(&reaped);
        let reaper = thread::Builder::new()
            .name(format!("mcp-reaper-{pid}"))
            .spawn(move || {
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
            });
        let process = Self {
            pid,
            exited,
            reaped,
        };
        if let Err(e) = reaper {
            // The child moved into the failed closure and was dropped unreaped; kill it at
            // least, so it does not keep running.
            process.kill_tree();
            return Err(e);
        }
        Ok((process, stdout, stdin))
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
