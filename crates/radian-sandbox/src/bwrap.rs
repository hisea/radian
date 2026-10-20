//! The Linux backend: bubblewrap (`bwrap`).
//!
//! bubblewrap is the only backend that satisfies `.git` read-only inside a writable workspace
//! (mount-level `--ro-bind` carve-outs; Landlock cannot express it — its rules are a union of
//! allows). It needs an unprivileged user namespace or a setuid installation; when neither is
//! available the probe fails and the mode is refused, never downgraded.
//!
//! The whole host filesystem is mounted read-only and the workspace is bound back writable,
//! so an escape attempt fails with a permission error rather than a missing path — the
//! control in the harness proves the target really exists.
//!
//! Unix sockets are **not** restricted by this backend (the accepted gap, ADR 0016 revision
//! 1 as decided for Linux): pathname sockets stay reachable through the shared read-only
//! `--ro-bind / /`, and abstract sockets through the shared network namespace in the
//! `workspace` tier. `[sandbox] allow_unix_sockets` therefore has
//! no effect on Linux; only macOS's seatbelt backend reads it.

use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::error::{ProbeReport, SandboxError};
use crate::policy::{GitAccess, SandboxMode, SandboxPolicy};
use crate::{SandboxBackend, workspace_tmp};

pub(crate) static BWRAP: Bwrap = Bwrap;

/// The launcher. Found on `PATH`; a distribution may install it setuid, which the probe
/// exercises exactly as a normal run would.
pub const BWRAP_BIN: &str = "bwrap";

pub struct Bwrap;

impl SandboxBackend for Bwrap {
    fn name(&self) -> &'static str {
        "linux-bwrap"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        match Command::new(BWRAP_BIN).arg("--version").output() {
            Ok(_) => {}
            Err(e) => {
                return Err(SandboxError::BackendUnavailable {
                    backend: self.name(),
                    detail: format!("{BWRAP_BIN} could not be run: {e}"),
                });
            }
        }
        let dir = crate::probe_dir()?;
        let ws = dir.join("ws");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&ws)?;
        std::fs::create_dir_all(&outside)?;
        let ws = std::fs::canonicalize(&ws)?;
        let outside = std::fs::canonicalize(&outside)?;
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace: ws.clone(),
            network: false,
            read_only: Vec::new(),
            git_access: GitAccess::default(),
            deny_read: Vec::new(),
            // The probe policy only needs the workspace bind; the unix-socket whitelist is empty
            // (the Linux backend does not restrict unix sockets, see the module docs).
            allow_unix_sockets: Vec::new(),
            allow_env: vec!["PATH".into()],
        };
        let inside_file = ws.join("ok.txt");
        let outside_file = outside.join("bad.txt");
        let script = format!(
            "printf ok > {}; printf bad > {}",
            shell_quote(&inside_file),
            shell_quote(&outside_file)
        );
        let mut command = self.command(
            &policy,
            OsStr::new("/bin/sh"),
            &[OsString::from("-c"), OsString::from(script)],
        )?;
        let output = command
            .output()
            .map_err(|e| SandboxError::BackendUnavailable {
                backend: self.name(),
                detail: format!("could not run the probe: {e}"),
            })?;
        let leaked = outside_file.exists();
        let inside_ok = inside_file.exists();
        let _ = std::fs::remove_dir_all(&dir);
        if leaked {
            return Err(SandboxError::ProbeFailed {
                backend: self.name(),
                detail: "a write outside the workspace was permitted".into(),
            });
        }
        if !inside_ok {
            return Err(SandboxError::ProbeFailed {
                backend: self.name(),
                detail: format!(
                    "bubblewrap could not set up the sandbox (exit {:?}): {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        Ok(ProbeReport {
            backend: self.name(),
            detail: "a write outside was denied and a write inside succeeded".into(),
        })
    }

    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<Command, SandboxError> {
        let mut command = Command::new(BWRAP_BIN);
        command.arg("--die-with-parent").arg("--new-session");
        if policy.network {
            // Keep the network namespace; isolate everything else.
            command.args([
                "--unshare-user",
                "--unshare-ipc",
                "--unshare-pid",
                "--unshare-uts",
                "--unshare-cgroup",
            ]);
        } else {
            command.arg("--unshare-all");
        }
        // Read the whole host filesystem, write only the workspace.
        command.args(["--ro-bind", "/", "/"]);
        command.args(["--proc", "/proc", "--dev", "/dev"]);
        if policy.mode == SandboxMode::WorkspaceWrite {
            command
                .arg("--bind")
                .arg(&policy.workspace)
                .arg(&policy.workspace);
            // ADR 0038 slice 1 is macOS-only: the whole discovered git metadata stays
            // `--ro-bind` read-only here, whatever `policy.git_access` says (Linux is pending
            // and nothing about it is claimed without running the VM matrix). The macOS
            // backend is where `WritableExceptExec` opens `.git` except for the exec entries.
            for path in &policy.read_only {
                command.arg("--ro-bind-try").arg(path).arg(path);
            }
        }
        // Sensitive reads: mask the path. A directory becomes an empty tmpfs; a file is
        // replaced by the null device. Only paths that exist can be masked: bwrap cannot
        // create a mount point on the read-only root, and a missing path has nothing to
        // hide yet. (SBPL can state a rule for a path that does not exist yet; bwrap
        // cannot. See docs/ for the difference.)
        for path in &policy.deny_read {
            if !path.exists() {
                continue;
            }
            if path.is_dir() {
                command.arg("--tmpfs").arg(path);
            } else {
                command.arg("--ro-bind").arg("/dev/null").arg(path);
            }
        }
        command
            .arg("--chdir")
            .arg(&policy.workspace)
            .arg("--clearenv");
        // HOME and TMPDIR are directories inside the workspace (the root is read-only and
        // `/tmp` must not be masked, since the workspace may live under it). PATH comes from
        // the parent when present.
        let tmp = workspace_tmp(policy);
        let home = policy.workspace.join(".radian-sandbox").join("home");
        let _ = std::fs::create_dir_all(&home);
        set_env(&mut command, "PATH", "/usr/bin:/bin");
        command.arg("--setenv").arg("HOME").arg(&home);
        command.arg("--setenv").arg("TMPDIR").arg(&tmp);
        command.arg("--setenv").arg("LANG").arg("C");
        for name in &policy.allow_env {
            if matches!(name.as_str(), "PATH" | "HOME" | "TMPDIR" | "LANG") {
                continue;
            }
            if let Some(value) = std::env::var_os(name) {
                command.arg("--setenv").arg(name).arg(value);
            }
        }
        command.arg("--").arg(program).args(args);
        Ok(command)
    }
}

/// Sets `name` to the parent's value when it exists, otherwise to `fallback`.
fn set_env(command: &mut Command, name: &str, fallback: &str) {
    let value = std::env::var_os(name).unwrap_or_else(|| fallback.into());
    command.arg("--setenv").arg(name).arg(value);
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Linux backend never claims to restrict Unix sockets (the accepted gap): pathname
    /// sockets stay reachable through the shared `--ro-bind / /`, and abstract sockets
    /// through the shared network namespace in `workspace` — so the `<runtime>` layer must
    /// carry no deny sentence there. The old tier-driven flag said otherwise on this
    /// machine.
    #[test]
    fn the_linux_backend_does_not_restrict_unix_sockets() {
        let ws = std::env::temp_dir().join(format!("radian-bwrap-sockets-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        for network in [true, false] {
            let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &ws)
                .unwrap()
                .network(network);
            assert!(
                !BWRAP.restricts_unix_sockets(&policy),
                "network = {network}"
            );
        }
        let _ = std::fs::remove_dir_all(&ws);
    }
}
