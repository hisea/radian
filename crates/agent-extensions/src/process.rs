//! Spawning and supervising one server process.
//!
//! The server runs in its own process group (Unix), so killing it also kills anything it
//! started. A dedicated OS thread owns the `Child` and blocks in `wait()`, which reaps the
//! process without needing a Tokio runtime; killing is a synchronous signal, so it also works
//! from `Drop` while a runtime is shutting down.

use std::io;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::watch;

use crate::config::McpServerConfig;

pub(crate) struct ServerProcess {
    pid: u32,
    exited: watch::Receiver<bool>,
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
        let reaper = thread::Builder::new()
            .name(format!("mcp-reaper-{pid}"))
            .spawn(move || {
                let _ = child.wait();
                let _ = exited_tx.send(true);
            });
        let process = Self { pid, exited };
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

    /// Sends SIGKILL to the whole process group. Synchronous and safe to call repeatedly.
    ///
    /// The group is signalled even after the leader exits, to reach children it left behind.
    /// A group id stays reserved while any member is alive, so this cannot hit an unrelated
    /// group unless the group is already empty and its id was reused.
    pub(crate) fn kill_tree(&self) {
        #[cfg(unix)]
        {
            if let Ok(pgid) = libc::pid_t::try_from(self.pid) {
                // SAFETY: killpg takes plain integers and has no memory-safety preconditions.
                unsafe {
                    libc::killpg(pgid, libc::SIGKILL);
                }
            }
        }
        #[cfg(windows)]
        {
            // Interim: terminate the tree by pid. Job Object supervision is planned.
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
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

impl Drop for ServerProcess {
    fn drop(&mut self) {
        // The reaper thread keeps running and reaps the process after the kill.
        self.kill_tree();
    }
}
