//! The permission profile and the sandbox: the two layers that decide *what may run* and
//! *what the OS enforces* (ADR 0012).
//!
//! This module is also the single place that turns a profile name into a
//! `radian_permissions::Profile` and a `radian_sandbox::Sandbox`. The two invariants it
//! exists to keep:
//!
//! - **`full-access` is the default** (ADR 0043 decision 2): [`default_profile`] returns it,
//!   because the neighbouring harnesses run unsandboxed by default and every real dogfood did.
//!   `--profile workspace` and a project's own `.radian/config.toml` turn the sandbox
//!   on; a project can only narrow, never widen.
//! - **A failed probe never degrades.** For the enforced mode, [`build_sandbox`] returns an
//!   error when the backend is missing or its probe fails; it never returns `None` so the
//!   caller could run unsandboxed.

use std::path::Path;

use radian_permissions::{
    Action, ApprovalPolicy, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_sandbox::{GitAccess, Sandbox, SandboxError, SandboxMode, SandboxPolicy};

use crate::SetupError;

/// The `--profile` value. A product concept (ADR 0012 P1), not a backend flag. The names are
/// `workspace`/`full` (ADR 0016); the old `workspace-write`/`full-access` are accepted
/// aliases but are deliberately **not** listed in `--help`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ProfileArg {
    #[value(name = "workspace", alias = "workspace-write")]
    WorkspaceWrite,
    #[value(name = "full", alias = "full-access")]
    FullAccess,
}

impl ProfileArg {
    pub(crate) fn preset(self) -> Preset {
        match self {
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
        "workspace" | "workspace-write" => Ok(ProfileArg::WorkspaceWrite),
        "full" | "full-access" => Ok(ProfileArg::FullAccess),
        other => Err(SetupError(format!("unknown profile {other:?}"))),
    }
}

/// The [`ProfileArg`] for a [`Preset`] (the reverse of [`ProfileArg::preset`]).
pub(crate) fn from_preset(preset: Preset) -> ProfileArg {
    match preset {
        Preset::WorkspaceWrite => ProfileArg::WorkspaceWrite,
        Preset::FullAccess => ProfileArg::FullAccess,
    }
}

/// The default profile: **`full-access`** (ADR 0043 decision 2).
///
/// Rationale: the neighbour harnesses (Claude Code, opencode, Gemini CLI) do not turn the OS
/// sandbox on by default, every dogfood run here used `--profile full`, and the built-in file
/// tools keep their own workspace boundary either way. The sandbox stays available and is the
/// recommended choice for an unfamiliar repository or an unattended run; `--profile workspace`,
/// a remembered per-project choice, or the project's own `.radian/config.toml` all turn it on
/// (a project can only narrow).
pub(crate) fn default_profile() -> ProfileArg {
    ProfileArg::FullAccess
}

/// Builds the permission profile: the preset's rules, plus any host allow rules for shell
/// commands. The preset is never changed; extra rules are added on top (ADR 0012: a host may
/// add `allow` rules, and only at the subcommand level — see
/// `crates/radian-permissions/README.md`).
///
/// Slice 1 of `sandbox-auto-allow-shell`: `sandboxed` is the sandbox's effective state —
/// `true` when the caller just built one (a probed local backend, or `inherited`) — so
/// the `workspace` rule is static for the session: `allow` inside the sandbox, `ask`
/// without one (fail closed). The caller builds the sandbox first and passes
/// `sandbox.is_some()`; a failed probe is the error that refuses the run, never a
/// fallback.
pub(crate) fn build_profile(
    profile: ProfileArg,
    workspace: &Path,
    allow_shell_commands: &[String],
    sandboxed: bool,
) -> Result<Profile, SetupError> {
    let preset = profile.preset();
    let base = match preset {
        Preset::WorkspaceWrite if sandboxed => Profile::workspace_write_sandboxed(workspace),
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
    // removed, so an allowlist can never widen a tier past what the preset allows.
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
        .map(|profile| profile.with_sandboxed(sandboxed && preset == Preset::WorkspaceWrite))
        .map_err(|e| SetupError(format!("--workspace {}: {e}", workspace.display())))
}

/// Builds the sandbox for `profile`.
///
/// The enforced mode builds a sandbox for the workspace; a probe failure is an **error**,
/// never a fallback.
///
/// The one other path is ADR 0016: in the `workspace` tier, when the kernel says
/// this process is already confined by an outer sandbox, the outer sandbox is inherited and
/// nothing is probed ([`Sandbox::inherited`]). That is not a fallback: the process cannot be
/// less confined than the parent that confined it, and no flag or variable selects it.
pub(crate) fn build_sandbox(
    profile: ProfileArg,
    workspace: &Path,
    deny_read: &[std::path::PathBuf],
    allow_unix_sockets: &[std::path::PathBuf],
    allow_env: &[String],
) -> Result<Option<Sandbox>, SetupError> {
    // ADR 0016: a process the **kernel** says is already confined by an
    // outer sandbox inherits it instead of stacking a second one. The nested probe cannot run
    // at all (the kernel refuses `sandbox_apply` under a deny-default profile), so this comes
    // before it. `confined_by_outer_sandbox` is the only source of that fact — no environment
    // variable, config key or flag can reach here (decision 1) — and a `false` answer falls
    // through to the probe and the refusal below, unchanged.
    if profile == ProfileArg::WorkspaceWrite && radian_sandbox::confined_by_outer_sandbox() {
        return Ok(Some(Sandbox::inherited(workspace)));
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
    // sandbox. `full` has no backend to enforce it.
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
    // ADR 0034: the user-level `[sandbox] allow_env` names, on top of
    // the built-in whitelist (which already carries the six `HERDR_*` names).
    for name in allow_env {
        policy = policy.allow_env(name.clone());
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
         if you really mean to run without a sandbox (the built-in default)",
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

    /// The config file's `permissions.profile` names: the two tiers plus the old aliases.
    /// Anything else — including the removed `safe` tier — is refused, never guessed.
    #[test]
    fn parse_profile_accepts_only_the_two_tiers_and_their_aliases() {
        for name in ["workspace", "workspace-write", "full", "full-access"] {
            assert!(parse_profile(name).is_ok(), "{name}");
        }
        for name in ["safe", "bogus"] {
            assert!(parse_profile(name).is_err(), "{name} must be refused");
        }
    }

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
        let result = build_sandbox(ProfileArg::WorkspaceWrite, &dir, &[], &[], &[]);
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
