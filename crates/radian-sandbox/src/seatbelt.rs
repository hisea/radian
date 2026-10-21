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
use crate::git_exec::{
    GIT_EXEC_DIRS, GIT_EXEC_FILES, GIT_MODULE_DIRS, GIT_MODULE_FILES, GIT_WORKTREE_FILES,
};
use crate::policy::{GitAccess, SandboxMode, SandboxPolicy};
use crate::{SandboxBackend, SandboxCommand, apply_env, workspace_tmp};

pub(crate) static SEATBELT: Seatbelt = Seatbelt;

/// The system launcher. `man sandbox-exec` marks it deprecated; it is still the only way to
/// apply a profile to an arbitrary child.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub struct Seatbelt;

impl SandboxBackend for Seatbelt {
    fn name(&self) -> &'static str {
        "macos-seatbelt"
    }

    /// macos-seatbelt is where the ADR 0038 carve-out is implemented, so it reports the policy's
    /// request as the effective one: `ReadOnly` keeps the whole metadata read-only, and
    /// `WritableExceptExec` opens it except the executable entries (`git_exec_denies`). The two
    /// stay in step because the same `policy.git_access` drives the profile and this answer.
    fn git_writable_except_exec(&self, policy: &SandboxPolicy) -> bool {
        matches!(policy.git_access, GitAccess::WritableExceptExec)
    }

    /// macos-seatbelt is where the ADR 0016 Unix-socket deny is implemented, so it
    /// reports the policy's network flag as the effective answer: the
    /// `(deny network-outbound (remote unix-socket))` rule is only emitted with
    /// `(allow network*)`, and without the network every outbound socket is already denied.
    /// The `<runtime>` prompt layer reads this via
    /// [`crate::Sandbox::restricts_unix_sockets`], so the sentence stays exactly as today.
    fn restricts_unix_sockets(&self, policy: &SandboxPolicy) -> bool {
        policy.network
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
    ) -> Result<SandboxCommand, SandboxError> {
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
        Ok(SandboxCommand::new(command))
    }
}

/// The SBPL profile for a policy. `(deny default)` with selective allows; the workspace is
/// the only writable subtree, and deeper `deny` rules carve read-only and no-read holes out
/// of it. `mach-lookup` is deliberately wide (ADR 0012 P4): the IPC boundary
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
        // entries git can execute. A linked worktree's per-worktree git dir and its shared
        // common dir usually live **outside** the workspace (ADR 0043 decision 1), so under
        // `WritableExceptExec` every discovered git **directory** first gets its own allow:
        // without it native `git commit` cannot write the index, refs or object store.
        if policy.git_access == GitAccess::WritableExceptExec {
            for path in &policy.read_only {
                if path.is_dir() {
                    profile.push_str(&format!(
                        "(allow file-write* (subpath {}))\n",
                        sbpl_string(&path.to_string_lossy())
                    ));
                }
            }
        }
        // Every deny follows every allow: the per-worktree git dir sits **under** the common
        // dir, so the common dir's `worktrees/<name>/...` deny must come after the allow of the
        // nested git dir, and no allow may come after a deny (SBPL's last match would win).
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
/// [`GitAccess::WritableExceptExec`] (ADR 0038). The entry names come from the shared list
/// (`crate::git_exec`); the permission layer denies the same entries for `write`/`patch`,
/// so the two cannot drift apart. Everything else under `.git` stays writable.
///
/// A submodule's directory is matched with a regex over `modules/<name>` rather than by
/// enumerating the names found at discovery time: a submodule the model adds during the
/// session must be covered too, and the same goes for a worktree it adds.
///
/// A worktree's `.git` is a pointer **file**, not a directory. It has no executable entry of
/// its own, but writing it would let the model repoint git at a gitdir it controls, whose
/// hooks then run outside the sandbox when the user next runs git; so it keeps the **full**
/// deny, exactly as under [`GitAccess::ReadOnly`]..
fn git_exec_denies(profile: &mut String, path: &Path) {
    if !path.is_dir() {
        profile.push_str(&format!("(deny file-write* {})\n", target(path)));
        return;
    }
    let base = path.to_string_lossy();
    let quoted = |suffix: &str| sbpl_string(&format!("{base}{suffix}"));
    for file in GIT_EXEC_FILES {
        profile.push_str(&format!(
            "(deny file-write* (literal {}))\n",
            quoted(&format!("/{file}"))
        ));
    }
    // `subpath` (not `literal`): the whole hooks directory and everything below it, and it
    // binds a `hooks/` that does not exist yet.
    for dir in GIT_EXEC_DIRS {
        profile.push_str(&format!(
            "(deny file-write* (subpath {}))\n",
            quoted(&format!("/{dir}"))
        ));
    }
    let escaped = regex_escape(&base);
    // Nested submodules (`modules/<a>/modules/<b>/config`): one or more `modules/<name>`
    // segments. The top-level entries are the literal and subpath rules above, so this
    // needs at least one segment (a `+`, not a `*`).
    for file in GIT_MODULE_FILES {
        profile.push_str(&format!(
            "(deny file-write* (regex {}))\n",
            sbpl_string(&format!(
                "^{escaped}(/modules/[^/]+)+/{}$",
                regex_escape(file)
            ))
        ));
    }
    for dir in GIT_MODULE_DIRS {
        profile.push_str(&format!(
            "(deny file-write* (regex {}))\n",
            sbpl_string(&format!(
                "^{escaped}(/modules/[^/]+)+/{}(/.*)?$",
                regex_escape(dir)
            ))
        ));
    }
    // A linked worktree of this git dir, or of a nested submodule's: the shared common dir
    // holds one `worktrees/<name>/` per worktree, and its link files and per-worktree config
    // are executables by the same argument (a repointed link, or a `core.hooksPath` in the
    // per-worktree config). Matched by name so a worktree added during the session is
    // covered without re-discovery.
    for name in GIT_WORKTREE_FILES {
        profile.push_str(&format!(
            "(deny file-write* (regex {}))\n",
            sbpl_string(&format!(
                "^{escaped}(/modules/[^/]+)*/worktrees/[^/]+/{}$",
                regex_escape(name)
            ))
        ));
    }
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

    /// ADR 0038 decision 4: the effective git access the per-session text reports is the
    /// policy's on this backend, because macos-seatbelt is where the carve-out is implemented.
    /// `ReadOnly` (the default) must report `false` so the text
    /// stays the read-only wording; only `WritableExceptExec` reports the writable one.
    #[test]
    fn the_effective_git_access_follows_the_policy() {
        let mut p = policy();
        assert!(
            !SEATBELT.git_writable_except_exec(&p),
            "the default must not claim the carve-out"
        );
        p.git_access = GitAccess::WritableExceptExec;
        assert!(
            SEATBELT.git_writable_except_exec(&p),
            "WritableExceptExec must report the carve-out"
        );
    }

    /// ADR 0038: `WritableExceptExec` keeps `.git` writable except the entries git can
    /// execute. The whole-directory deny must be gone, and the exec rules must be present with
    /// the right shapes: `literal` for config/config.worktree, `subpath` for `hooks/`, a regex
    /// over nested `modules/<name>` segments for submodules, and regexes for a linked
    /// worktree's `gitdir`/`commondir`/`config.worktree`.
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
        // The submodule rules are a regex over one or more `modules/<name>` segments (a nested
        // submodule is covered without re-discovery); the path's `.` is regex-escaped and then
        // SBPL-escaped, so it appears as `\\.` in the profile source.
        assert!(profile.contains("(/modules/[^/]+)+/config$"), "{profile}");
        assert!(
            profile.contains("(/modules/[^/]+)+/hooks(/.*)?$"),
            "{profile}"
        );
        assert!(
            profile.contains(r"\\.git(/modules/"),
            "the path dot must be regex-escaped and SBPL-escaped:\n{profile}"
        );
        // The linked worktree's link files and per-worktree config, by name.
        assert!(profile.contains("/worktrees/[^/]+/gitdir$"), "{profile}");
        assert!(profile.contains("/worktrees/[^/]+/commondir$"), "{profile}");
        assert!(
            profile.contains(r"/worktrees/[^/]+/config\\.worktree$"),
            "{profile}"
        );
        // The whole git directory must NOT be read-only anymore.
        assert!(
            !profile.contains(&format!("(deny file-write* (subpath \"{quoted}\"))")),
            "the whole .git must stay writable under WritableExceptExec:\n{profile}"
        );
    }

    /// ADR 0016: the macOS backend reports the policy's network flag as the
    /// effective Unix-socket answer — the `(deny network-outbound (remote unix-socket))`
    /// rule is only emitted with `(allow network*)`, so a `workspace` session keeps
    /// today's `<runtime>` sentence and this wiring changes nothing there.
    #[test]
    fn the_effective_unix_socket_answer_follows_the_network_flag() {
        let mut p = policy();
        assert!(
            !SEATBELT.restricts_unix_sockets(&p),
            "no network, no Unix-socket deny to enforce"
        );
        p.network = true;
        assert!(
            SEATBELT.restricts_unix_sockets(&p),
            "the network-on profile denies outbound Unix sockets"
        );
    }

    /// A worktree's `.git` is a pointer **file**: it has no executable entry of its own, but
    /// writing it would repoint git at a gitdir the model controls, so under
    /// `WritableExceptExec` it keeps the full deny (a `literal`, exactly as `ReadOnly` does).
    #[test]
    fn a_worktree_pointer_file_keeps_the_full_deny() {
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
            profile.contains(&format!(
                "(deny file-write* (literal \"{}\"))",
                pointer.to_string_lossy()
            )),
            "a worktree pointer file must keep the full read-only deny:\n{profile}"
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
    fn paths_with_quotes_are_escaped() {
        assert_eq!(sbpl_string("a\"b"), "\"a\\\"b\"");
    }
}
