//! The macOS backend: `sandbox-exec` with a generated SBPL profile.
//!
//! SBPL operation names are a private interface that drifts between macOS releases, and
//! `sandbox-exec` is deprecated. Profile generation is therefore kept in one place, and the
//! backend is only used after [`Seatbelt::probe`] has run a real child and proven the deny
//! and the workspace allow both take effect.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use crate::error::{ProbeReport, SandboxError};
use crate::policy::{SandboxMode, SandboxPolicy};
use crate::{SandboxBackend, apply_env, workspace_tmp};

pub(crate) static SEATBELT: Seatbelt = Seatbelt;

/// The system launcher. `man sandbox-exec` marks it deprecated; it is still the only way to
/// apply a profile to an arbitrary child.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub struct Seatbelt;

impl SandboxBackend for Seatbelt {
    fn name(&self) -> &'static str {
        "macos-seatbelt"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        if !Path::new(SANDBOX_EXEC).exists() {
            return Err(SandboxError::BackendUnavailable {
                backend: self.name(),
                detail: format!("{SANDBOX_EXEC} does not exist"),
            });
        }
        let dir = probe_dir()?;
        let ws = dir.join("ws");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&ws)?;
        std::fs::create_dir_all(&outside)?;
        let ws = std::fs::canonicalize(&ws)?;
        let outside = std::fs::canonicalize(&outside)?;
        // The probe uses the exact policy shape a real run uses, minus the HOME-based deny
        // list (the probe must not depend on the user's home contents).
        let policy = SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace: ws.clone(),
            network: false,
            read_only: Vec::new(),
            deny_read: Vec::new(),
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
                detail: "a write outside the workspace was permitted; the profile is not \
                         being enforced"
                    .into(),
            });
        }
        if !inside_ok {
            return Err(SandboxError::ProbeFailed {
                backend: self.name(),
                detail: format!(
                    "the workspace was not writable (exit {:?}): {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        if output.status.success() {
            return Err(SandboxError::ProbeFailed {
                backend: self.name(),
                detail: "the denied write reported success".into(),
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
        let profile = profile_text(policy);
        let mut command = Command::new(SANDBOX_EXEC);
        command
            .arg("-p")
            .arg(profile)
            .arg("--")
            .arg(program)
            .args(args)
            .current_dir(&policy.workspace);
        apply_env(&mut command, policy);
        if policy.mode == SandboxMode::WorkspaceWrite {
            // Tools write temporary files; keep them inside the one writable tree.
            command.env("TMPDIR", workspace_tmp(policy));
        }
        Ok(command)
    }
}

/// The SBPL profile for a policy. `(deny default)` with selective allows; the workspace is
/// the only writable subtree, and deeper `deny` rules carve read-only and no-read holes out
/// of it. `mach-lookup` is deliberately wide (ADR 0012 decision 4): the IPC boundary
/// is **not** tightened, and no text may claim credential isolation because of this profile.
pub(crate) fn profile_text(policy: &SandboxPolicy) -> String {
    let mut profile = String::new();
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n");
    profile.push_str("(allow file-read*)\n");
    profile.push_str("(allow file-test-existence)\n");
    profile.push_str("(allow process-exec*)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow sysctl-read)\n");
    if policy.network {
        profile.push_str("(allow network*)\n");
    }
    // Sensitive reads: a deeper deny overrides the broad `file-read*` above.
    for path in &policy.deny_read {
        // Both forms: the path may be a directory (subpath covers its future contents) or a
        // file (literal matches it exactly).
        let escaped = sbpl_string(&path.to_string_lossy());
        profile.push_str(&format!("(deny file-read* (literal {escaped}))\n"));
        profile.push_str(&format!("(deny file-read* (subpath {escaped}))\n"));
    }
    // `/dev/null` must be writable for git and friends.
    profile.push_str("(allow file-write-data (literal \"/dev/null\"))\n");
    if policy.mode == SandboxMode::WorkspaceWrite {
        profile.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_string(&policy.workspace.to_string_lossy())
        ));
        // Read-only carve-outs (git metadata): deeper deny over the workspace allow.
        for path in &policy.read_only {
            profile.push_str(&format!("(deny file-write* {})\n", target(path)));
        }
    }
    profile
}

/// A directory is matched with `subpath`, a file with `literal` (a worktree pointer file is
/// a regular file, and `subpath` would not match it).
fn target(path: &Path) -> String {
    if path.is_dir() {
        format!("(subpath {})", sbpl_string(&path.to_string_lossy()))
    } else {
        format!("(literal {})", sbpl_string(&path.to_string_lossy()))
    }
}

fn sbpl_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

pub(crate) fn probe_dir() -> Result<std::path::PathBuf, SandboxError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-sandbox-probe-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxPolicy;

    fn policy() -> SandboxPolicy {
        let ws = std::env::temp_dir();
        SandboxPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace: ws,
            network: false,
            read_only: vec![std::path::PathBuf::from("/tmp/ws/.git")],
            deny_read: vec![std::path::PathBuf::from("/Users/x/.ssh")],
            allow_env: vec!["PATH".into()],
        }
    }

    #[test]
    fn the_profile_denies_by_default_and_carves_out_git_and_sensitive_reads() {
        let profile = profile_text(&policy());
        assert!(profile.contains("(deny default)"));
        // The path does not exist in this unit test, so it is a literal carve-out.
        assert!(profile.contains("(deny file-write* (literal \"/tmp/ws/.git\"))"));
        assert!(profile.contains("(deny file-read* (subpath \"/Users/x/.ssh\"))"));
        assert!(
            !profile.contains("(allow network*)"),
            "network is off by default"
        );
    }

    #[test]
    fn network_is_only_opened_when_asked() {
        let mut p = policy();
        p.network = true;
        assert!(profile_text(&p).contains("(allow network*)"));
    }

    #[test]
    fn safe_mode_does_not_allow_any_workspace_write() {
        let mut p = policy();
        p.mode = SandboxMode::Safe;
        let profile = profile_text(&p);
        assert!(!profile.contains("(allow file-write* (subpath"));
    }

    #[test]
    fn paths_with_quotes_are_escaped() {
        assert_eq!(sbpl_string("a\"b"), "\"a\\\"b\"");
    }
}
