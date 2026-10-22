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
//! The Linux backend: bubblewrap (`bwrap`) with a seccomp filter in the `workspace` tier.
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
//! Unix sockets **are** restricted by this backend in the `workspace` tier (slice 0): a
//! seccomp filter (`crate::seccomp`) denies `socket(AF_UNIX, …)` and `io_uring_setup` with
//! `EPERM`, covering pathname and abstract sockets alike (both need `socket` first, and
//! seccomp cannot see paths, so there is no per-path whitelist on Linux).
//! `[sandbox] allow_unix_sockets` therefore still has no effect on Linux — only macOS's
//! seatbelt backend reads it — and a non-empty list warns once at startup.
//!
//! `WritableExceptExec` (ADR 0038) is implemented with bind mounts: every discovered git
//! directory is bound writable, and then the entries git can execute are bound back
//! read-only. bwrap has no regex rules, so the exec entries are enumerated from what exists
//! ([`git_exec_entries`]); see `LIMITS.md` for the one gap that leaves.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{ProbeReport, SandboxError};
use crate::git_exec::{
    GIT_EXEC_DIRS, GIT_EXEC_FILES, GIT_MODULE_DIRS, GIT_MODULE_FILES, GIT_WORKTREE_FILES,
};
use crate::policy::{GitAccess, SandboxMode, SandboxPolicy};
use crate::seccomp;
use crate::{SandboxBackend, SandboxCommand, workspace_tmp};

pub(crate) static BWRAP: Bwrap = Bwrap;

/// The launcher. Found on `PATH`; a distribution may install it setuid, which the probe
/// exercises exactly as a normal run would.
pub const BWRAP_BIN: &str = "bwrap";

pub struct Bwrap;

impl SandboxBackend for Bwrap {
    fn name(&self) -> &'static str {
        "linux-bwrap"
    }

    /// bubblewrap implements ADR 0038's carve-out with bind mounts, so the policy's request is
    /// the effective one: `ReadOnly` keeps the whole discovered metadata read-only, and
    /// `WritableExceptExec` binds each git directory writable and then binds the executable
    /// entries back read-only. The per-session text may therefore promise native `git commit`
    /// in the `workspace` tier on Linux, exactly as it does on macOS.
    fn git_writable_except_exec(&self, policy: &SandboxPolicy) -> bool {
        matches!(policy.git_access, GitAccess::WritableExceptExec)
    }

    /// The Linux backend denies outbound Unix sockets in the `workspace` tier (slice 0): the
    /// seccomp filter rejects `socket(AF_UNIX, …)` outright, so the `<runtime>` prompt layer
    /// reports the deny through [`crate::Sandbox::restricts_unix_sockets`]. There is no
    /// per-path whitelist on Linux — seccomp cannot see paths — so the sentence's whitelist
    /// clause only ever names macOS-allowed sockets; a non-empty `allow_unix_sockets` warns
    /// once at startup that it is ignored on Linux.
    fn restricts_unix_sockets(&self, policy: &SandboxPolicy) -> bool {
        policy.mode == SandboxMode::WorkspaceWrite
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
            // The probe policy has an empty whitelist; on Linux the key is ignored anyway
            // (the seccomp filter denies every Unix socket, so there is nothing to allow).
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
    ) -> Result<SandboxCommand, SandboxError> {
        let mut command = Command::new(BWRAP_BIN);
        command.arg("--die-with-parent").arg("--new-session");
        // Slice 0: the seccomp filter fd, owned by the returned command (see
        // `crate::seccomp::pipe_for_program` for the lifetime guarantee). No `pre_exec`
        // hook: the shell tool registers its own `pre_exec` for `setsid` afterwards,
        // which would replace one set here (measured: the second hook wins).
        let mut seccomp_filter = None;
        if policy.mode == SandboxMode::WorkspaceWrite {
            // Deny `socket(AF_UNIX, …)` and `io_uring_setup` with `EPERM`.
            let (filter, number) = seccomp::pipe_for_program()?;
            command.arg("--seccomp").arg(number.to_string());
            seccomp_filter = Some(filter);
        }
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
            match policy.git_access {
                // The task-agent rule: the whole discovered metadata stays read-only.
                GitAccess::ReadOnly => {
                    for path in &policy.read_only {
                        command.arg("--ro-bind-try").arg(path).arg(path);
                    }
                }
                // ADR 0038: open each discovered git directory, then close the entries git can
                // execute. Every allow is mounted before every deny: a later mount wins, and a
                // per-worktree git dir sits **under** its shared common dir.
                GitAccess::WritableExceptExec => {
                    for path in &policy.read_only {
                        if path.is_dir() {
                            command.arg("--bind").arg(path).arg(path);
                        }
                    }
                    for path in &policy.read_only {
                        if path.is_dir() {
                            for entry in git_exec_entries(path) {
                                command.arg("--ro-bind-try").arg(&entry).arg(&entry);
                            }
                        } else {
                            // A worktree's `.git` is a pointer **file**: writing it would
                            // repoint git at a gitdir the model controls, whose hooks then run
                            // outside the sandbox, so it keeps the full deny.
                            command.arg("--ro-bind-try").arg(path).arg(path);
                        }
                    }
                }
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
        match seccomp_filter {
            Some(filter) => Ok(SandboxCommand::with_seccomp_filter(command, filter)),
            None => Ok(SandboxCommand::with_cleared_child_env(command)),
        }
    }
}

/// Sets `name` to the parent's value when it exists, otherwise to `fallback`.
fn set_env(command: &mut Command, name: &str, fallback: &str) {
    let value = std::env::var_os(name).unwrap_or_else(|| fallback.into());
    command.arg("--setenv").arg(name).arg(value);
}

/// The concrete paths under one discovered git directory that must stay read-only under
/// [`GitAccess::WritableExceptExec`] (ADR 0038): this git directory's own exec files and
/// `hooks/`, the same entries of every existing submodule git directory (at any depth), and
/// the link files and per-worktree config of every existing linked worktree.
///
/// bwrap has no regex rules, so this enumerates what exists at the time the command is built.
/// A path that does not exist is skipped: a bind mount would have to create the destination on
/// the host filesystem (the same reason `deny_read` skips missing paths). See `LIMITS.md`.
fn git_exec_entries(git_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_exec_entries(git_dir, GIT_EXEC_FILES, GIT_EXEC_DIRS, &mut out);
    collect_worktree_entries(&git_dir.join("worktrees"), &mut out);
    collect_module_entries(&git_dir.join("modules"), &mut out);
    out
}

fn collect_exec_entries(git_dir: &Path, files: &[&str], dirs: &[&str], out: &mut Vec<PathBuf>) {
    for name in files.iter().chain(dirs.iter()) {
        push_existing(git_dir.join(name), out);
    }
}

/// `<git>/worktrees/<name>/{gitdir,commondir,config.worktree}`: a repointed link would make
/// git use another gitdir, and a `core.hooksPath` in the per-worktree config would run when the
/// user next runs git there.
fn collect_worktree_entries(worktrees: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(worktrees) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type()` is the directory entry's `d_type` and never follows a symlink.
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for name in GIT_WORKTREE_FILES {
            push_existing(entry.path().join(name), out);
        }
    }
}

/// Every existing submodule git directory: `<git>/modules/<name>/` and, recursively,
/// `modules/<a>/modules/<b>/`. A submodule's executable entries are its `config` and `hooks/`
/// (not `config.worktree`, per ADR 0038 decision 3), plus any linked worktree of its own.
fn collect_module_entries(modules: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(modules) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let git_dir = entry.path();
        collect_exec_entries(&git_dir, GIT_MODULE_FILES, GIT_MODULE_DIRS, out);
        collect_worktree_entries(&git_dir.join("worktrees"), out);
        collect_module_entries(&git_dir.join("modules"), out);
    }
}

fn push_existing(path: PathBuf, out: &mut Vec<PathBuf>) {
    if std::fs::symlink_metadata(&path).is_ok() && !out.contains(&path) {
        out.push(path);
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Linux backend restricts Unix sockets in the `workspace` tier (slice 0): the
    /// seccomp filter denies `socket(AF_UNIX, …)` outright. The old tier-driven claim said
    /// otherwise; a filter that denies every Unix socket must be reported as a deny, or the
    /// `<runtime>` layer would hide it from the model. Removing the `--seccomp` install above
    /// (or the `AF_UNIX` rule in the filter) breaks the escape matrix's `unix_socket` cases.
    #[test]
    fn the_linux_backend_restricts_unix_sockets_in_the_workspace_tier() {
        let ws = std::env::temp_dir().join(format!("radian-bwrap-sockets-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        for network in [true, false] {
            let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &ws)
                .unwrap()
                .network(network);
            assert!(BWRAP.restricts_unix_sockets(&policy), "network = {network}");
            // The deny is installed for the tier regardless of the network flag: with the
            // network off, pathname sockets would otherwise stay reachable through the
            // shared read-only root.
            let command = BWRAP
                .command(
                    &policy,
                    std::ffi::OsStr::new("/bin/true"),
                    &[] as &[std::ffi::OsString],
                )
                .unwrap();
            let argv: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let seccomp_at = argv.iter().position(|arg| arg == "--seccomp");
            assert!(seccomp_at.is_some(), "workspace gets --seccomp: {argv:?}");
        }
        let full = SandboxPolicy::new(SandboxMode::FullAccess, &ws).unwrap();
        assert!(!BWRAP.restricts_unix_sockets(&full));
        let command = BWRAP
            .command(
                &full,
                std::ffi::OsStr::new("/bin/true"),
                &[] as &[std::ffi::OsString],
            )
            .unwrap();
        assert!(
            !command.get_args().any(|arg| arg == "--seccomp"),
            "full runs no sandbox and gets no filter"
        );
    }

    /// ADR 0038: under `WritableExceptExec` every discovered git directory is bound writable
    /// **before** its executable entries are bound back read-only, and the whole-directory
    /// `--ro-bind-try` is gone. The exec entries come from the shared list, so a missing one
    /// (no `config.worktree` here) is skipped rather than mounted.
    #[test]
    fn writable_except_exec_binds_git_writable_then_the_exec_entries_read_only() {
        let root = std::env::temp_dir().join(format!("radian-bwrap-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let git = root.join("repo").join(".git");
        let per = git.join("worktrees").join("wt");
        std::fs::create_dir_all(&per).unwrap();
        std::fs::create_dir_all(git.join("hooks")).unwrap();
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("modules").join("sub").join("hooks")).unwrap();
        std::fs::write(git.join("config"), "[core]\n").unwrap();
        std::fs::write(git.join("modules").join("sub").join("config"), "").unwrap();
        std::fs::write(per.join("gitdir"), "gitdir: elsewhere\n").unwrap();
        std::fs::write(per.join("commondir"), "../..\n").unwrap();
        let ws = root
            .join("repo")
            .join(".radian")
            .join("arc")
            .join("worktrees")
            .join("wt");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".git"), format!("gitdir: {}\n", per.display())).unwrap();

        let policy = |access| {
            let mut policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &ws).unwrap();
            policy.git_access = access;
            policy.read_only = vec![ws.join(".git"), per.clone(), git.clone()];
            policy
        };
        let argv = |policy: &SandboxPolicy| -> Vec<String> {
            BWRAP
                .command(policy, OsStr::new("/bin/true"), &[] as &[OsString])
                .unwrap()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        };

        let writable = argv(&policy(GitAccess::WritableExceptExec));
        let find = |argv: &[String], flag: &str, path: &Path| {
            argv.windows(3).position(|window| {
                window[0] == flag && window[1] == path.to_string_lossy().as_ref()
            })
        };
        let git_bind = find(&writable, "--bind", &git)
            .unwrap_or_else(|| panic!("no --bind for .git: {writable:?}"));
        let pointer = find(&writable, "--ro-bind-try", &ws.join(".git"))
            .unwrap_or_else(|| panic!("{writable:?}"));
        assert!(
            !writable
                .windows(3)
                .any(|w| w[0] == "--ro-bind-try" && w[1] == git.to_string_lossy().as_ref()),
            "the whole .git must not stay read-only: {writable:?}"
        );
        for path in [
            git.join("config"),
            git.join("hooks"),
            git.join("modules").join("sub").join("config"),
            git.join("modules").join("sub").join("hooks"),
            per.join("gitdir"),
            per.join("commondir"),
        ] {
            let at = find(&writable, "--ro-bind-try", &path)
                .unwrap_or_else(|| panic!("no deny for {}: {writable:?}", path.display()));
            assert!(
                git_bind < at,
                "every allow precedes every deny: {writable:?}"
            );
        }
        assert!(git_bind < pointer);
        // Not an executable entry: the object store stays writable (no mount over it).
        assert!(
            find(&writable, "--ro-bind-try", &git.join("objects")).is_none(),
            "{writable:?}"
        );

        let read_only = argv(&policy(GitAccess::ReadOnly));
        assert!(
            read_only
                .windows(3)
                .any(|w| w[0] == "--ro-bind-try" && w[1] == git.to_string_lossy().as_ref()),
            "ReadOnly keeps the whole .git read-only: {read_only:?}"
        );
        assert!(find(&read_only, "--bind", &git).is_none(), "{read_only:?}");
        // The pointer file keeps the full deny in both modes.
        assert!(
            read_only
                .windows(3)
                .any(|w| w[0] == "--ro-bind-try"
                    && w[1] == ws.join(".git").to_string_lossy().as_ref())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The effective answer follows the policy now that both the allow and the exec denies are
    /// mounted, so the `shell` description and the startup line may promise native git.
    #[test]
    fn the_effective_git_access_follows_the_policy() {
        let ws = std::env::temp_dir();
        let mut policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &ws).unwrap();
        assert!(!BWRAP.git_writable_except_exec(&policy));
        policy.git_access = GitAccess::WritableExceptExec;
        assert!(BWRAP.git_writable_except_exec(&policy));
    }
}
