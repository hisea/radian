//! The permission profile and the sandbox: the two layers that decide *what may run* and
//! *what the OS enforces* (ADR 0012).
//!
//! This module is also the single place that turns a profile name into a
//! `radian_permissions::Profile` and a `radian_sandbox::Sandbox`. The two invariants it
//! exists to keep:
//!
//! - **`full-access` is never a default.** [`default_profile`] returns `workspace-write`;
//!   `full-access` is only reachable by naming it on the command line.
//! - **A failed probe never degrades.** For an enforced mode, [`build_sandbox`] returns an
//!   error when the backend is missing or its probe fails; it never returns `None` so the
//!   caller could run unsandboxed.

use std::path::Path;

use radian_permissions::{
    Action, ApprovalPolicy, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_sandbox::{GitAccess, Sandbox, SandboxError, SandboxMode, SandboxPolicy};

use crate::SetupError;

/// The `--profile` value. A product concept (ADR 0012 P1), not a backend flag. The names are
/// `safe`/`workspace`/`full` (ADR 0016); the old `workspace-write`/`full-access` are accepted
/// aliases but are deliberately **not** listed in `--help`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ProfileArg {
    Safe,
    #[value(name = "workspace", alias = "workspace-write")]
    WorkspaceWrite,
    #[value(name = "full", alias = "full-access")]
    FullAccess,
}

impl ProfileArg {
    pub(crate) fn preset(self) -> Preset {
        match self {
            Self::Safe => Preset::Safe,
            Self::WorkspaceWrite => Preset::WorkspaceWrite,
            Self::FullAccess => Preset::FullAccess,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        self.preset().as_str()
    }
}

/// The config file's `permissions.profile` name. The config reader has already restricted it
/// to the known names; this maps them onto the CLI's values (old aliases included).
pub(crate) fn parse_profile(name: &str) -> Result<ProfileArg, SetupError> {
    match name {
        "safe" => Ok(ProfileArg::Safe),
        "workspace" | "workspace-write" => Ok(ProfileArg::WorkspaceWrite),
        "full" | "full-access" => Ok(ProfileArg::FullAccess),
        other => Err(SetupError(format!("unknown profile {other:?}"))),
    }
}

/// The [`ProfileArg`] for a [`Preset`] (the reverse of [`ProfileArg::preset`]).
pub(crate) fn from_preset(preset: Preset) -> ProfileArg {
    match preset {
        Preset::Safe => ProfileArg::Safe,
        Preset::WorkspaceWrite => ProfileArg::WorkspaceWrite,
        Preset::FullAccess => ProfileArg::FullAccess,
    }
}

/// The default profile: **`workspace-write`**, never `full-access`.
///
/// Rationale: it is the least surprising useful mode. The built-in file tools are confined by
/// their own workspace boundary, `shell` exists only when a workspace is given (and then runs
/// under the sandbox), and MCP tools keep working the way this CLI has always run them.
/// `safe` would silently turn every MCP tool into a denial, and `full-access` must never be
/// something a user gets by forgetting a flag.
pub(crate) fn default_profile() -> ProfileArg {
    ProfileArg::WorkspaceWrite
}

/// Builds the permission profile: the preset's rules, plus any host allow rules for shell
/// commands. The preset is never changed; extra rules are added on top (ADR 0012: a host may
/// add `allow` rules, and only at the subcommand level — see
/// `crates/radian-permissions/README.md`).
pub(crate) fn build_profile(
    profile: ProfileArg,
    workspace: &Path,
    allow_shell_commands: &[String],
) -> Result<Profile, SetupError> {
    let preset = profile.preset();
    let base = match preset {
        Preset::Safe => Profile::safe(workspace),
        Preset::WorkspaceWrite => Profile::workspace_write(workspace),
        Preset::FullAccess => Profile::full_access(workspace),
    }
    .map_err(|e| SetupError(format!("--workspace {}: {e}", workspace.display())))?;
    if allow_shell_commands.is_empty() {
        return Ok(base);
    }
    // The preset's blanket "shell asks" rule would outrank every allow rule (`evaluate` keeps
    // the highest-ranked action among matches), so an allowlist *replaces* it: a shell command
    // that matches no allow rule then falls to the default `ask`. `deny` rules are never
    // removed, so `--profile safe` still denies everything.
    let mut rules: Vec<Rule> = base
        .rules()
        .iter()
        .filter(|rule| {
            !(matches!(&rule.tool, ToolMatcher::Name(name) if name == "shell")
                && rule.action == Action::Ask)
        })
        .cloned()
        .collect();
    for raw in allow_shell_commands {
        let prefix: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
        if prefix.is_empty() {
            return Err(SetupError(
                "--allow-shell-command needs a command prefix, e.g. `git status`".into(),
            ));
        }
        rules.push(
            Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::CommandPrefix(prefix),
                Action::Allow,
            )
            .with_reason("allowed by --allow-shell-command"),
        );
    }
    Profile::custom(preset, ApprovalPolicy::OnAsk, workspace, rules)
        .map_err(|e| SetupError(format!("--workspace {}: {e}", workspace.display())))
}

/// Builds the sandbox for `profile`.
///
/// `safe` mounts no `shell` and its rules deny every execution path, so there is nothing to
/// sandbox and `None` is returned. `workspace-write` and `full-access` build a sandbox for
/// the workspace; for the enforced mode a probe failure is an **error**, never a fallback.
pub(crate) fn build_sandbox(
    profile: ProfileArg,
    workspace: &Path,
    deny_read: &[std::path::PathBuf],
    allow_unix_sockets: &[std::path::PathBuf],
) -> Result<Option<Sandbox>, SetupError> {
    if profile == ProfileArg::Safe {
        return Ok(None);
    }
    let mode = profile.preset().sandbox_mode();
    if probe_failure_injected() {
        return Err(SetupError(unavailable(profile, mode)));
    }
    let mut policy = match SandboxPolicy::new(mode, workspace) {
        Ok(policy) => policy,
        Err(error) => {
            return Err(SetupError(format!(
                "{}: {error}",
                unavailable(profile, mode)
            )));
        }
    };
    // ADR 0038 decision 4: a general (and, later, lead) session in the `workspace` tier may
    // write `.git` except the entries git can execute, so `git commit` and friends work in the
    // sandbox. `full` has no backend to enforce it and `safe` builds no sandbox.
    if profile == ProfileArg::WorkspaceWrite {
        policy = policy.git_access(GitAccess::WritableExceptExec);
    }
    // ADR 0020 decision 3: the actual sessions directory and the memory directory are not
    // readable in the enforced tiers. `full` gets the list too, but its transparent backend ignores it.
    for path in deny_read {
        policy = policy.deny_read(path);
    }
    // ADR 0016: the user's whitelisted Unix sockets.
    for path in allow_unix_sockets {
        policy = policy.allow_unix_socket(path);
    }
    match Sandbox::from_policy(policy) {
        Ok(sandbox) => Ok(Some(sandbox)),
        // The workspace being inside a denied directory is not a backend problem: say what it
        // is and how to fix it, not "install/fix the backend" (dogfooding D1's lesson).
        Err(error @ SandboxError::InvalidWorkspace { .. }) => Err(SetupError(error.to_string())),
        Err(error) => Err(SetupError(format!(
            "{}: {error}",
            unavailable(profile, mode)
        ))),
    }
}

fn unavailable(profile: ProfileArg, mode: SandboxMode) -> String {
    format!(
        "the `{}` sandbox backend is unavailable or failed its probe, so `{}` cannot be \
         enforced; refusing to run. Install/fix the backend, or pass `--profile full` \
         if you really mean to run without a sandbox (that is never the default)",
        mode.as_str(),
        profile.as_str()
    )
}

/// A test seam: when `RADIAN_SANDBOX_PROBE_FAIL=1`, the CLI behaves exactly as if the probe
/// had failed. It can only cause a refusal, never an unsandboxed run, so it cannot weaken the
/// guarantee; it exists so the "probe failure does not degrade" path is testable without
/// removing the real backend from the machine.
fn probe_failure_injected() -> bool {
    std::env::var("RADIAN_SANDBOX_PROBE_FAIL").is_ok_and(|value| value == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0038 decision 4: the `workspace` tier a general (and later lead) session runs in
    /// must let it write `.git` except the exec entries. If a host forgets to ask for it, the
    /// policy falls back to the read-only default and native `git commit` stops working.
    #[test]
    fn the_workspace_tier_asks_for_writable_except_exec() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("radian-cli-profile-git-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = build_sandbox(ProfileArg::WorkspaceWrite, &dir, &[], &[]);
        let _ = std::fs::remove_dir_all(&dir);
        match result {
            Ok(Some(sandbox)) => assert_eq!(
                sandbox.policy().git_access,
                GitAccess::WritableExceptExec,
                "the workspace tier must request WritableExceptExec"
            ),
            // No working backend on this machine: there is no policy to inspect.
            Err(_) => {}
            Ok(None) => panic!("the workspace tier must build a sandbox"),
        }
    }
}
