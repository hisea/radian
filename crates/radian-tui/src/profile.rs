//! The permission profile and the sandbox: what may run, and what the OS enforces (ADR 0012).
//!
//! This mirrors `agent-cli`'s wiring on purpose — the same presets from `radian-permissions`,
//! the same sandbox construction from `radian-sandbox`, the same two invariants:
//!
//! - **`full-access` is never a default.** [`parse_tier`] and the CLI default pick
//!   `workspace-write`; `full-access` is only reachable by naming it.
//! - **A failed probe never degrades.** [`build_sandbox`] returns an error for an enforced
//!   tier when the backend is missing or its probe fails; it never returns `None` so a caller
//!   could run unsandboxed.

use std::path::Path;

use radian_permissions::{
    Action, ApprovalPolicy, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};

/// The `--profile` value: a product concept (ADR 0012 P1), not a backend flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Safe,
    WorkspaceWrite,
    FullAccess,
}

impl Tier {
    pub fn preset(self) -> Preset {
        match self {
            Self::Safe => Preset::Safe,
            Self::WorkspaceWrite => Preset::WorkspaceWrite,
            Self::FullAccess => Preset::FullAccess,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.preset().as_str()
    }
}

pub fn parse_tier(value: &str) -> Result<Tier, String> {
    match value {
        "safe" => Ok(Tier::Safe),
        "workspace" | "workspace-write" => Ok(Tier::WorkspaceWrite),
        "full" | "full-access" => Ok(Tier::FullAccess),
        other => Err(format!(
            "--profile {other:?} is not one of safe, workspace, full"
        )),
    }
}

/// The permission profile: the preset's rules, plus host `allow` rules for shell commands.
///
/// The preset is never changed; extra rules are added on top. An allowlist *replaces* the
/// preset's blanket "shell asks" rule, because `evaluate` keeps the highest-ranked action and
/// an `ask` would otherwise outrank every `allow`. A command matching no allow rule then falls
/// to the default `ask`, and `deny` rules are never removed.
pub fn build_profile(
    tier: Tier,
    workspace: &Path,
    allow_shell_commands: &[String],
) -> Result<Profile, String> {
    let preset = tier.preset();
    let base = match preset {
        Preset::Safe => Profile::safe(workspace),
        Preset::WorkspaceWrite => Profile::workspace_write(workspace),
        Preset::FullAccess => Profile::full_access(workspace),
    }
    .map_err(|e| format!("--workspace {}: {e}", workspace.display()))?;
    if allow_shell_commands.is_empty() {
        return Ok(base);
    }
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
            return Err(
                "--allow-shell-command needs a command prefix, e.g. `git status`".to_string(),
            );
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
        .map_err(|e| format!("--workspace {}: {e}", workspace.display()))
}

/// Builds the sandbox for `tier`.
///
/// `safe` mounts no `shell` and its rules deny every execution path, so there is nothing to
/// sandbox and `None` is returned. `workspace-write` and `full-access` build a sandbox for the
/// workspace; for the enforced tier a probe failure is an **error**, never a fallback.
pub fn build_sandbox(
    tier: Tier,
    workspace: &Path,
    deny_read: &[std::path::PathBuf],
) -> Result<Option<Sandbox>, String> {
    if tier == Tier::Safe {
        return Ok(None);
    }
    let mode = tier.preset().sandbox_mode();
    if probe_failure_injected() {
        return Err(unavailable(tier, mode));
    }
    let mut policy = SandboxPolicy::new(mode, workspace)
        .map_err(|error| format!("{}: {error}", unavailable(tier, mode)))?;
    // ADR 0020 decision 3: the actual sessions directory and the memory directory are not
    // readable in the enforced tiers. `full` gets the list too, but its transparent backend ignores it.
    for path in deny_read {
        policy = policy.deny_read(path);
    }
    match Sandbox::from_policy(policy) {
        Ok(sandbox) => Ok(Some(sandbox)),
        Err(error) => Err(format!("{}: {error}", unavailable(tier, mode))),
    }
}

fn unavailable(tier: Tier, mode: SandboxMode) -> String {
    format!(
        "the `{}` sandbox backend is unavailable or failed its probe, so `{}` cannot be \
         enforced; refusing to run. Install/fix the backend, or pass `--profile full-access` \
         if you really mean to run without a sandbox (that is never the default)",
        mode.as_str(),
        tier.as_str()
    )
}

/// A test seam: with `RADIAN_SANDBOX_PROBE_FAIL=1` the TUI behaves exactly as if the probe had
/// failed. It can only cause a refusal, never an unsandboxed run, so it cannot weaken the
/// guarantee; it exists so the "probe failure does not degrade" path is testable without
/// removing the real backend from the machine.
fn probe_failure_injected() -> bool {
    std::env::var("RADIAN_SANDBOX_PROBE_FAIL").is_ok_and(|value| value == "1")
}

/// The two startup lines: what was chosen, and what it actually means. The wording follows
/// ADR 0012 decision 4 and must **not** claim credential isolation: the macOS backend
/// leaves `mach-lookup` open, so IPC is not contained.
pub fn describe(tier: Tier, sandbox: Option<&Sandbox>) -> (String, String) {
    // The shared text lives in `radian-host` (dogfooding D5); this host keeps only its
    // wording: the full-screen host talks about its shell.
    radian_host::describe(
        tier.preset().sandbox_mode(),
        sandbox,
        radian_host::TUI_WORDING,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_names_and_aliases_parse() {
        for (name, tier) in [
            ("safe", Tier::Safe),
            ("workspace", Tier::WorkspaceWrite),
            ("workspace-write", Tier::WorkspaceWrite),
            ("full", Tier::FullAccess),
            ("full-access", Tier::FullAccess),
        ] {
            assert_eq!(parse_tier(name).unwrap(), tier, "{name}");
        }
        let error = parse_tier("bogus").unwrap_err();
        assert!(error.contains("workspace"), "{error}");
        assert!(error.contains("full"), "{error}");
        assert!(!error.contains("workspace-write"), "{error}");
        assert!(!error.contains("full-access"), "{error}");
    }
}
