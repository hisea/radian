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
use crate::policy::{GitAccess, SandboxMode, SandboxPolicy};
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
        let dir = crate::probe_dir()?;
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
            git_access: GitAccess::default(),
            deny_read: Vec::new(),
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
    // Signals only within this sandbox instance: a sandboxed command may stop its own
    // children and grandchildren (test suites, build tools and timeouts need this), but not a
    // process outside the sandbox nor one in another instance of the same profile. Measured
    // 2026-10-13 on macOS 27 with `sandbox-exec`: `(target children)` misses grandchildren;
    // `(target pgrp)` and a bare `(allow signal)` reach a same-group process outside the
    // sandbox, so neither may be used. The escape matrix's `signals` group holds this.
    profile.push_str("(allow signal (target same-sandbox))\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow sysctl-read)\n");
    if policy.network {
        profile.push_str("(allow network*)\n");
        // ADR 0016: `(allow network*)` also allows connecting to **any** same-user
        // Unix socket (herdr, ssh-agent, docker.sock). Deny outbound Unix sockets, then allow
        // the exceptions. SBPL: a later rule wins, so this order is the whole fix.
        profile.push_str("(deny network-outbound (remote unix-socket))\n");
        // macOS resolves names through mDNSResponder; without this DNS fails.
        profile.push_str(&format!(
            "(allow network-outbound (remote unix-socket (path-literal {})))\n",
            sbpl_string(MDNS_RESPONDER)
        ));
        for path in &policy.allow_unix_sockets {
            profile.push_str(&format!(
                "(allow network-outbound (remote unix-socket (path-literal {})))\n",
                sbpl_string(&path.to_string_lossy())
            ));
        }
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
        // The pty the tools need (ADR 0016). `openpty` needs to write both the master and the
        // slave and to ioctl them; a bare `(allow pseudo-tty)` or an unfiltered `file-ioctl`
        // is not used. Measured on macOS 27: this filtered pair makes `os.openpty()` work.
        profile.push_str(PTY_ALLOW);
        // Git metadata carve-outs (a deeper deny over the workspace allow, so SBPL's
        // last-match rule makes them win). `ReadOnly` keeps the whole `.git` (or worktree
        // pointer file) read-only; `WritableExceptExec` (ADR 0038) opens it except for the
        // entries git can execute.
        for path in &policy.read_only {
            match policy.git_access {
                GitAccess::ReadOnly => {
                    profile.push_str(&format!("(deny file-write* {})\n", target(path)));
                }
                GitAccess::WritableExceptExec => git_exec_denies(&mut profile, path),
            }
        }
    }
    profile
}

/// The entries under one discovered git directory that git can execute, denied write under
/// [`GitAccess::WritableExceptExec`] (ADR 0038): `config`, `config.worktree`, `hooks/`, and
/// under `modules/<name>/` each submodule's `config` and `hooks/`. Everything else under
/// `.git` stays writable.
///
/// A submodule's directory is matched with a regex over `modules/<name>` rather than by
/// enumerating the names found at discovery time: a submodule the model adds during the
/// session must be covered too. A worktree's `.git` **file** is not a directory and has no
/// executable entry of its own; the per-worktree git dir and the shared common dir the pointer
/// names are discovered separately and passed through this same function.
fn git_exec_denies(profile: &mut String, path: &Path) {
    if !path.is_dir() {
        return;
    }
    let base = path.to_string_lossy();
    let quoted = |suffix: &str| sbpl_string(&format!("{base}{suffix}"));
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted("/config")
    ));
    profile.push_str(&format!(
        "(deny file-write* (literal {}))\n",
        quoted("/config.worktree")
    ));
    // `subpath` (not `literal`): the whole hooks directory and everything below it, and it
    // binds a `hooks/` that does not exist yet.
    profile.push_str(&format!(
        "(deny file-write* (subpath {}))\n",
        quoted("/hooks")
    ));
    let escaped = regex_escape(&base);
    profile.push_str(&format!(
        "(deny file-write* (regex {}))\n",
        sbpl_string(&format!("^{escaped}/modules/[^/]+/config$"))
    ));
    profile.push_str(&format!(
        "(deny file-write* (regex {}))\n",
        sbpl_string(&format!("^{escaped}/modules/[^/]+/hooks(/.*)?$"))
    ));
}

/// Escapes the regex metacharacters a filesystem path may contain, so it matches literally
/// inside an SBPL `(regex ...)`. `/` is not special and is left alone.
fn regex_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(
            c,
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// macOS's DNS resolver socket (ADR 0016): always allowed when the network is on.
const MDNS_RESPONDER: &str = "/private/var/run/mDNSResponder";

/// The pty rules for the `workspace` tier. `file-ioctl` is filtered to exactly the pty
/// devices, so it cannot be used to ioctl anything else (TIOCSTI is then blocked by the
/// kernel's controlling-terminal check plus the shell tool's `setsid`; see the matrix).
const PTY_ALLOW: &str = concat!(
    "(allow file-write* (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\"))\n",
    "(allow file-ioctl (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\"))\n",
);

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
            git_access: GitAccess::default(),
            deny_read: vec![std::path::PathBuf::from("/Users/x/.ssh")],
            allow_unix_sockets: Vec::new(),
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

    /// ADR 0038: `WritableExceptExec` keeps `.git` writable except the entries git can
    /// execute. The whole-directory deny must be gone, and the four exec rules must be
    /// present with the right shapes: `literal` for config/config.worktree, `subpath` for
    /// `hooks/`, and a regex over `modules/<name>` for submodules.
    #[test]
    fn writable_except_exec_carves_out_only_the_exec_entries() {
        let dir = std::env::temp_dir().join(format!(
            "radian-seatbelt-profile-{}-git",
            std::process::id()
        ));
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        let mut p = policy();
        p.read_only = vec![git.clone()];
        p.git_access = GitAccess::WritableExceptExec;
        let profile = profile_text(&p);
        let _ = std::fs::remove_dir_all(&dir);

        let quoted = git.to_string_lossy();
        assert!(
            profile.contains(&format!("(deny file-write* (literal \"{quoted}/config\"))")),
            "{profile}"
        );
        assert!(
            profile.contains(&format!(
                "(deny file-write* (literal \"{quoted}/config.worktree\"))"
            )),
            "{profile}"
        );
        assert!(
            profile.contains(&format!("(deny file-write* (subpath \"{quoted}/hooks\"))")),
            "{profile}"
        );
        // The submodule rules are a regex over the name (a later submodule is covered without
        // re-discovery); the path's `.` is regex-escaped and then SBPL-escaped, so it appears
        // as `\\.` in the profile source.
        assert!(profile.contains("/modules/[^/]+/config$"), "{profile}");
        assert!(profile.contains("/modules/[^/]+/hooks(/.*)?$"), "{profile}");
        assert!(
            profile.contains(r"\\.git/modules/"),
            "the path dot must be regex-escaped and SBPL-escaped:\n{profile}"
        );
        // The whole git directory must NOT be read-only anymore.
        assert!(
            !profile.contains(&format!("(deny file-write* (subpath \"{quoted}\"))")),
            "the whole .git must stay writable under WritableExceptExec:\n{profile}"
        );
    }

    /// A worktree's `.git` is a pointer **file**: it is discovered and passed here, but it has
    /// no executable entry of its own, so under `WritableExceptExec` it gets no carve-out (its
    /// per-worktree git dir and shared common dir are separate discovered entries).
    #[test]
    fn a_worktree_pointer_file_gets_no_exec_carve_out() {
        let dir = std::env::temp_dir().join(format!(
            "radian-seatbelt-profile-{}-pointer",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pointer = dir.join(".git");
        std::fs::write(&pointer, "gitdir: /elsewhere\n").unwrap();
        let mut p = policy();
        p.read_only = vec![pointer.clone()];
        p.git_access = GitAccess::WritableExceptExec;
        let profile = profile_text(&p);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !profile.contains(&format!("(literal \"{}\")", pointer.to_string_lossy())),
            "a pointer file is not a directory and must not be carved out:\n{profile}"
        );
    }

    #[test]
    fn the_pty_ioctl_allow_is_filtered_to_the_pty_devices() {
        let profile = profile_text(&policy());
        // The ioctl allow is filtered: an unfiltered `(allow file-ioctl)` would open every
        // device, which ADR 0016 forbids even though the escape matrix cannot tell them apart.
        assert!(
            profile.contains(
                "(allow file-ioctl (literal \"/dev/ptmx\") (regex #\"^/dev/ttys[0-9]+$\"))"
            ),
            "{profile}"
        );
        assert!(!profile.contains("(allow file-ioctl)\n"), "{profile}");
        assert!(
            profile.contains("(allow file-write* (literal \"/dev/ptmx\")"),
            "{profile}"
        );
    }

    #[test]
    fn signals_are_confined_to_the_same_sandbox() {
        let profile = profile_text(&policy());
        assert!(profile.contains("(allow signal (target same-sandbox))"));
        // Any wider signal rule would let a sandboxed command signal the host.
        assert_eq!(profile.matches("(allow signal").count(), 1);
    }

    #[test]
    fn network_is_only_opened_when_asked() {
        let mut p = policy();
        p.network = true;
        assert!(profile_text(&p).contains("(allow network*)"));
    }

    /// ADR 0016: with the network on, outbound Unix sockets are denied, DNS
    /// (mDNSResponder) is always allowed, and each whitelisted socket follows the deny. The
    /// order is the fix (a later SBPL rule wins).
    #[test]
    fn unix_sockets_are_denied_with_the_dns_exception_and_the_whitelist() {
        let mut p = policy();
        p.network = true;
        p.allow_unix_sockets = vec![std::path::PathBuf::from("/Users/x/herdr.sock")];
        let profile = profile_text(&p);
        let deny = profile
            .find("(deny network-outbound (remote unix-socket))")
            .unwrap_or_else(|| panic!("no deny rule:\n{profile}"));
        let dns = profile
            .find("(remote unix-socket (path-literal \"/private/var/run/mDNSResponder\"))")
            .unwrap_or_else(|| panic!("no mDNSResponder allow:\n{profile}"));
        let allowed = profile
            .find("(remote unix-socket (path-literal \"/Users/x/herdr.sock\"))")
            .unwrap_or_else(|| panic!("no whitelist allow:\n{profile}"));
        assert!(deny < dns, "the deny must come first");
        assert!(dns < allowed, "the whitelist must follow the deny");

        // With the network off there is no rule at all (everything is already denied).
        let off = profile_text(&policy());
        assert!(!off.contains("network-outbound"), "{off}");
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
