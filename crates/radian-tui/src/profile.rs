//! The permission profile and the sandbox: what may run, and what the OS enforces (ADR 0012).
//!
//! This mirrors `agent-cli`'s wiring on purpose — the same presets from `radian-permissions`,
//! the same sandbox construction from `radian-sandbox`, the same two invariants:
//!
//! - **`full-access` is the default** (ADR 0043): [`parse_tier`] and the CLI default pick
//!   `full`; `workspace` is only reached by naming it, a remembered project choice,
//!   or a project's own `.radian/config.toml` (which can only narrow).
//! - **A failed probe never degrades.** [`build_sandbox`] returns an error for an enforced
//!   tier when the backend is missing or its probe fails; it never returns `None` so a caller
//!   could run unsandboxed.

use std::path::Path;

use radian_permissions::{
    Action, ApprovalPolicy, Preset, Profile, ResourceMatcher, Rule, ToolMatcher,
};
use radian_sandbox::{GitAccess, Sandbox, SandboxError, SandboxMode, SandboxPolicy};

/// The `--profile` value: a product concept (ADR 0012 P1), not a backend flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    WorkspaceWrite,
    FullAccess,
}

impl Tier {
    pub fn preset(self) -> Preset {
        match self {
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
        "workspace" | "workspace-write" => Ok(Tier::WorkspaceWrite),
        "full" | "full-access" => Ok(Tier::FullAccess),
        other => Err(format!("--profile {other:?} is not one of workspace, full")),
    }
}

/// The [`Tier`] for a [`Preset`] (the reverse of [`Tier::preset`]).
pub fn tier_of(preset: Preset) -> Tier {
    match preset {
        Preset::WorkspaceWrite => Tier::WorkspaceWrite,
        Preset::FullAccess => Tier::FullAccess,
    }
}

/// Where the tier in force at startup came from (ADR 0038 decision 3). `/access` with no
/// argument shows [`AccessSource::label`]; the startup summary and the footer append
/// [`AccessSource::suffix`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessSource {
    /// `--profile` on the command line.
    Flag,
    /// `permissions.profile` in the config file.
    Config,
    /// The remembered per-project record (`~/.radian/access/access.json`).
    Project,
    /// The resumed session's last `ConfigChanged` access.
    Session,
    /// The project declaration (`<workspace>/.radian/config.toml`) narrowed the tier.
    ProjectConfig,
    /// Nothing named a tier: the built-in default (`full`, ADR 0043).
    Default,
}

impl AccessSource {
    /// The source word `/access` prints, matching the task's "flag, config, or remembered".
    pub fn label(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Config => "config",
            Self::Project => "project",
            Self::Session => "session",
            Self::ProjectConfig => "project config",
            Self::Default => "default",
        }
    }

    /// The suffix the startup summary and the footer append. A named source is marked so the
    /// user can see why the tier is what it is; the built-in default stays bare.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Flag => " (flag)",
            Self::Config => " (config)",
            Self::Project => " (project)",
            Self::Session => " (session)",
            Self::ProjectConfig => " (project config)",
            Self::Default => "",
        }
    }
}

/// The footer's tier item: the tier plus where it came from. For `full` it says plainly
/// that nothing prompts (`full · no prompts`); `workspace` stays bare.
pub fn footer_tier(tier: Tier, source: AccessSource) -> String {
    match tier {
        Tier::FullAccess => format!("full · no prompts{}", source.suffix()),
        Tier::WorkspaceWrite => format!("{}{}", tier.as_str(), source.suffix()),
    }
}
/// profile with this host's `--allow-shell-command` rules, and the sandbox with the deny-read
/// list and the Unix-socket whitelist. A failed sandbox probe is the `Err` that makes
/// `/access` refuse (ADR 0012: a failed probe never degrades).
pub fn build_access(
    preset: Preset,
    workspace: &Path,
    allow_shell_commands: &[String],
    deny_read: &[std::path::PathBuf],
    allow_unix_sockets: &[std::path::PathBuf],
    allow_env: &[String],
) -> Result<radian_host::AccessParts, String> {
    let tier = tier_of(preset);
    let profile = build_profile(tier, workspace, allow_shell_commands)?;
    let sandbox = build_sandbox(tier, workspace, deny_read, allow_unix_sockets, allow_env)?;
    Ok(radian_host::AccessParts { profile, sandbox })
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
/// The enforced tier builds a sandbox for the workspace; a probe failure is an **error**,
/// never a fallback.
///
/// The one other path is ADR 0016: in the `workspace` tier, when the kernel says
/// this process is already confined by an outer sandbox, the outer sandbox is inherited and
/// nothing is probed ([`Sandbox::inherited`]). That is not a fallback: the process cannot be
/// less confined than the parent that confined it, and no flag or variable selects it.
pub fn build_sandbox(
    tier: Tier,
    workspace: &Path,
    deny_read: &[std::path::PathBuf],
    allow_unix_sockets: &[std::path::PathBuf],
    allow_env: &[String],
) -> Result<Option<Sandbox>, String> {
    // ADR 0016: a process the **kernel** says is already confined by an
    // outer sandbox inherits it instead of stacking a second one. The nested probe cannot run
    // at all (the kernel refuses `sandbox_apply` under a deny-default profile), so this comes
    // before it. `confined_by_outer_sandbox` is the only source of that fact — no environment
    // variable, config key or flag can reach here (decision 1) — and a `false` answer falls
    // through to the probe and the refusal below, unchanged.
    if tier == Tier::WorkspaceWrite && radian_sandbox::confined_by_outer_sandbox() {
        return Ok(Some(Sandbox::inherited(workspace)));
    }
    let mode = tier.preset().sandbox_mode();
    if probe_failure_injected() {
        return Err(unavailable(tier, mode));
    }
    let mut policy = SandboxPolicy::new(mode, workspace)
        .map_err(|error| format!("{}: {error}", unavailable(tier, mode)))?;
    // ADR 0038 decision 4: a general (and, later, lead) session in the `workspace` tier may
    // write `.git` except the entries git can execute, so `git commit` and friends work in the
    // sandbox. `full` has no backend to enforce it.
    if tier == Tier::WorkspaceWrite {
        policy = policy.git_access(GitAccess::WritableExceptExec);
    }
    // ADR 0020 decision 3: the actual sessions directory and the memory directory are not
    // readable in the enforced tiers. `full` gets the list too, but its transparent backend ignores it.
    for path in deny_read {
        policy = policy.deny_read(path);
    }
    // ADR 0016: the sockets the user whitelisted (already `~`-expanded and
    // canonicalized by the host). With the network off they are inert.
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
        Err(error @ SandboxError::InvalidWorkspace { .. }) => Err(error.to_string()),
        Err(error) => Err(format!("{}: {error}", unavailable(tier, mode))),
    }
}

fn unavailable(tier: Tier, mode: SandboxMode) -> String {
    format!(
        "the `{}` sandbox backend is unavailable or failed its probe, so `{}` cannot be \
         enforced; refusing to run. Install/fix the backend, or pass `--profile full` \
         if you really mean to run without a sandbox (the built-in default)",
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
            ("workspace", Tier::WorkspaceWrite),
            ("workspace-write", Tier::WorkspaceWrite),
            ("full", Tier::FullAccess),
            ("full-access", Tier::FullAccess),
        ] {
            assert_eq!(parse_tier(name).unwrap(), tier, "{name}");
        }
        for bogus in ["bogus", "safe"] {
            let error = parse_tier(bogus).unwrap_err();
            assert!(error.contains("workspace"), "{bogus}: {error}");
            assert!(error.contains("full"), "{bogus}: {error}");
            assert!(!error.contains("workspace-write"), "{bogus}: {error}");
            assert!(!error.contains("full-access"), "{bogus}: {error}");
        }
    }

    /// The footer's tier item says `no prompts` for `full` and stays bare for `workspace`.
    /// Dropping the words fails this test.
    #[test]
    fn the_footer_tier_says_no_prompts_for_full() {
        assert_eq!(
            footer_tier(Tier::FullAccess, AccessSource::Default),
            "full · no prompts"
        );
        assert_eq!(
            footer_tier(Tier::FullAccess, AccessSource::Project),
            "full · no prompts (project)"
        );
        assert_eq!(
            footer_tier(Tier::WorkspaceWrite, AccessSource::Session),
            "workspace (session)"
        );
    }

    /// ADR 0038 decision 4: the TUI's `workspace` tier must request `WritableExceptExec` so
    /// native `git commit` works in the sandbox. A host that forgets it silently drops back to
    /// the task-agent read-only rule.
    #[test]
    fn the_workspace_tier_asks_for_writable_except_exec() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("radian-tui-profile-git-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = build_sandbox(Tier::WorkspaceWrite, &dir, &[], &[], &[]);
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
