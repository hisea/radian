//! Named permission presets: the three layers of ADR 0012 as one profile.

use std::path::{Path, PathBuf};

use radian_sandbox::SandboxMode;

use crate::rule::{Action, ResourceMatcher, Rule, ToolMatcher};

/// How `ask` is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Ask the approver when a rule says `ask` (the default).
    OnAsk,
    /// Never ask: an `ask` becomes `deny`. For non-interactive runs.
    Never,
    /// Ask on every call a rule does not deny, overriding `allow`.
    Always,
    /// Auto-approve (ADR 0026 D9): an `ask` becomes `allow`. `deny` rules still deny and
    /// the sandbox still applies; only the question is removed. The same behavior `--yes`
    /// selects.
    Auto,
}

impl ApprovalPolicy {
    /// The `--approval` / `[permissions] approval` name (ADR 0012, ADR 0026 D9).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "on-ask" => Some(Self::OnAsk),
            "never" => Some(Self::Never),
            "always" => Some(Self::Always),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// The name [`ApprovalPolicy::parse`] accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnAsk => "on-ask",
            Self::Never => "never",
            Self::Always => "always",
            Self::Auto => "auto",
        }
    }
}

/// The named combinations the runtime selects between. `Preset` is the product concept; the
/// concrete `SandboxMode` it pairs with is deliberately part of the same struct, so a preset
/// cannot be swapped for a different execution boundary by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    WorkspaceWrite,
    FullAccess,
}

impl Preset {
    /// The user-visible tier name. `workspace-write`/`full-access` remain accepted as CLI
    /// aliases, but this is the name the hosts print and the status bar shows (ADR 0016).
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::WorkspaceWrite => "workspace",
            Preset::FullAccess => "full",
        }
    }

    pub fn sandbox_mode(self) -> SandboxMode {
        match self {
            Preset::WorkspaceWrite => SandboxMode::WorkspaceWrite,
            Preset::FullAccess => SandboxMode::FullAccess,
        }
    }
}

/// Why a profile could not be built (a workspace root that does not resolve).
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("{path}: cannot use as the permission workspace root: {source}")]
    Workspace {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: the permission workspace root is not a directory")]
    NotADirectory(PathBuf),
}

/// One permission profile: a preset's rule set plus its approval policy and sandbox mode.
///
/// `sandboxed` is the sandbox's effective state for the session (slice 1 of
/// `sandbox-auto-allow-shell`): `true` when the session's `workspace` shell calls run
/// inside a sandbox (a local backend whose probe succeeded, or ADR 0016's `inherited`),
/// in which case the tier's blanket shell rule is `allow`. It is static for the session —
/// the host builds the profile after the sandbox, so the rule never depends on a live
/// probe — and `false` keeps the fail-closed `ask` (a failed probe never becomes an
/// allow). Approval still cannot widen the sandbox (ADR 0012 P1): this only stops asking
/// for what the sandbox already confines.
#[derive(Debug, Clone)]
pub struct Profile {
    pub preset: Preset,
    pub sandbox: SandboxMode,
    pub approval: ApprovalPolicy,
    pub workspace: PathBuf,
    pub sandboxed: bool,
    rules: Vec<Rule>,
}

impl Profile {
    /// The `workspace` tier without a sandbox (or before the probe): shell asks.
    /// A failed probe never becomes an allow — fail closed.
    pub fn workspace_write(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::build(Preset::WorkspaceWrite, workspace, |ws| {
            let mut rules = read_only();
            rules.extend(memory_rules());
            rules.extend(always_allowed());
            rules.extend(write_under(ws));
            rules.extend([
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::Any,
                    Action::Ask,
                ),
                Rule::new(ToolMatcher::Mcp, ResourceMatcher::Any, Action::Ask),
            ]);
            rules
        })
    }

    /// The `workspace` tier with a sandbox in force: shell inside the sandbox is `allow`
    /// (a `deny` rule still wins first, and MCP stays `ask`). The caller must have built
    /// the sandbox already — a local backend whose probe succeeded, or `inherited` — and
    /// passes no proof here: the host's build order (sandbox first, then this) is what
    /// makes the rule mean "inside the sandbox". Using this without one is the mutation
    /// `workspace_sandboxed_shell_is_allowed_only_with_a_sandbox` catches.
    pub fn workspace_write_sandboxed(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let mut profile = Self::build(Preset::WorkspaceWrite, workspace, |ws| {
            let mut rules = read_only();
            rules.extend(memory_rules());
            rules.extend(always_allowed());
            rules.extend(write_under(ws));
            rules.extend([
                // Inside the sandbox: the OS boundary, not the prompt, is what confines
                // the command — so, like `full`, it covers every compound segment,
                // every `cd` (even one leaving the workspace) and every wrapper.
                // A `deny` rule still wins first, and a named `ask` rule still asks.
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::Any,
                    Action::Allow,
                ),
                Rule::new(ToolMatcher::Mcp, ResourceMatcher::Any, Action::Ask),
            ]);
            rules
        })?;
        profile.sandboxed = true;
        Ok(profile)
    }

    pub fn full_access(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::build(Preset::FullAccess, workspace, |ws| {
            let mut rules = read_only();
            rules.extend(memory_rules());
            rules.extend(always_allowed());
            rules.extend(write_under(ws));
            rules.extend([
                // `full` trusts the environment (like pi): no built-in prompt.
                // A user's own `deny`/`ask` rules still win (deny > ask > allow).
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::Any,
                    Action::Allow,
                ),
                Rule::new(ToolMatcher::Mcp, ResourceMatcher::Any, Action::Allow),
            ]);
            rules
        })
    }

    /// A profile with an explicit rule list (tests, and hosts with their own presets).
    pub fn custom(
        preset: Preset,
        approval: ApprovalPolicy,
        workspace: impl AsRef<Path>,
        rules: Vec<Rule>,
    ) -> Result<Self, ProfileError> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        Ok(Self {
            preset,
            sandbox: preset.sandbox_mode(),
            approval,
            workspace,
            sandboxed: false,
            rules,
        })
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Marks the profile as sandboxed after the fact (hosts that add their own rules
    /// with [`Profile::custom`] on top of a sandboxed session). Only ever narrows the
    /// question, never the boundary: approval still cannot widen the sandbox.
    pub fn with_sandboxed(mut self, sandboxed: bool) -> Self {
        self.sandboxed = sandboxed;
        self
    }

    /// Whether this session's `workspace` shell calls run inside a sandbox (slice 1 of
    /// `sandbox-auto-allow-shell`): the compound-segment check reads it, so a segment
    /// matched only by the tier's blanket `allow` still falls through to `ask`.
    pub fn is_sandboxed(&self) -> bool {
        self.sandboxed
    }

    /// Whether the tier's shell rule is a blanket `allow` (the `full` tier, or the
    /// `workspace` tier with a sandbox in force): the permission layer trusts it as
    /// the execution boundary, so a compound, a `cd` segment (even one leaving the
    /// workspace — the sandbox confines it) and a wrapper segment are judged like
    /// any other call there. Inspected from the rule set, not from the preset name,
    /// so a custom profile without the blanket rule never counts as blanket-allowed.
    pub fn shell_blanket_allow(&self) -> bool {
        self.rules.iter().any(|rule| {
            rule.action == Action::Allow
                && matches!(&rule.tool, ToolMatcher::Name(name) if name == "shell")
                && matches!(rule.resource, ResourceMatcher::Any)
        })
    }

    /// Allows these tool names in **every** tier, with the normal rule priority: tools such as
    /// `ask_user` and `skill` write no workspace file and run no repository command, so they
    /// never prompt. A `deny` rule still wins, and the host's [`crate::ToolGate`] is checked
    /// before the rules.
    pub fn with_allowed_tools(mut self, names: &[&str]) -> Self {
        self.rules.extend(names.iter().map(|name| {
            Rule::new(
                ToolMatcher::Name((*name).to_owned()),
                ResourceMatcher::Any,
                Action::Allow,
            )
        }));
        self
    }

    /// Replaces the approval policy (used by `custom` callers and tests).
    pub fn with_approval(mut self, approval: ApprovalPolicy) -> Self {
        self.approval = approval;
        self
    }

    /// Denies every tool in `names` (the allow-list gate). A session's tool allow-list is
    /// enforced with this: the host passes the built-ins the list does **not**
    /// name, and `deny` outranks `allow`/`ask`, so a listed tool the tier would not mount stays
    /// unmounted and an unlisted tool cannot run. It only ever adds `deny` rules, so it can
    /// narrow a tier but never widen one.
    pub fn with_denied_tools<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rules.extend(names.into_iter().map(|name| {
            Rule::new(
                ToolMatcher::Name(name.into()),
                ResourceMatcher::Any,
                Action::Deny,
            )
            .with_reason("not in this session's tool allow-list")
        }));
        self
    }

    fn build(
        preset: Preset,
        workspace: impl AsRef<Path>,
        rules: impl FnOnce(&Path) -> Vec<Rule>,
    ) -> Result<Self, ProfileError> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        let rules = rules(&workspace);
        Ok(Self {
            preset,
            sandbox: preset.sandbox_mode(),
            approval: ApprovalPolicy::OnAsk,
            workspace,
            sandboxed: false,
            rules,
        })
    }
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, ProfileError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| ProfileError::Workspace {
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(ProfileError::NotADirectory(canonical));
    }
    Ok(canonical)
}

/// The memory tool (ADR 0021 D5/D7). Reading is allowed in every tier. There is
/// deliberately **no** archive/forget/purge/confirm/save/revise agent tool (D5: the model has
/// no write path).
fn memory_rules() -> Vec<Rule> {
    [("memory_search", Action::Allow)]
        .into_iter()
        .map(|(name, action)| {
            Rule::new(ToolMatcher::Name(name.into()), ResourceMatcher::Any, action)
        })
        .collect()
}

/// The read-only built-ins, allowed regardless of path: `radian-coding` enforces the
/// workspace boundary itself for these tools, so the permission layer does not need to.
/// `git_status` and `list` are not here: they are one-version aliases judged as `git_diff`
/// and `read` (ADR 0043 decision 3).
fn read_only() -> Vec<Rule> {
    ["read", "glob", "grep", "git_diff"]
        .into_iter()
        .map(|name| {
            Rule::new(
                ToolMatcher::Name(name.into()),
                ResourceMatcher::Any,
                Action::Allow,
            )
        })
        .collect()
}

/// Fixed `allow` in every tier (ADR 0024 Q6 and ADR 0025 S3): `ask_user` writes nothing and
/// runs nothing, and `skill` only reads a `SKILL.md` that discovery already accepted — a skill
/// **is not a permission**, so reading one never prompts and grants nothing. Both still go
/// through the ordinary hook path. `subagent` (ADR 0043 decision 1a) is here for a different
/// reason: the tool itself writes no file and runs no workspace command; it starts a child
/// session whose own tier and approval decide everything it may do. A parent prompt would only
/// add a second gate that a headless parent cannot answer.
fn always_allowed() -> Vec<Rule> {
    ["ask_user", "skill", "subagent"]
        .into_iter()
        .map(|name| {
            Rule::new(
                ToolMatcher::Name(name.into()),
                ResourceMatcher::Any,
                Action::Allow,
            )
        })
        .collect()
}

/// File modification allowed only when the resolved path stays under the workspace.
fn write_under(workspace: &Path) -> Vec<Rule> {
    ["write", "patch"]
        .into_iter()
        .map(|name| {
            Rule::new(
                ToolMatcher::Name(name.into()),
                ResourceMatcher::PathUnder(workspace.to_path_buf()),
                Action::Allow,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolCall;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({}),
        }
    }

    fn action(profile: &Profile, name: &str) -> Action {
        crate::rule::evaluate(profile.rules(), &profile.workspace, &call(name)).action
    }

    /// ADR 0021 D5/D7: `memory_search` is allowed in both tiers; no memory tool asks.
    #[test]
    fn memory_search_is_allowed_in_every_tier_and_no_memory_tool_asks() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "memory_search"), Action::Allow, "{tier}");
            // No rule matches a write or destructive name, so the default `ask` stands —
            // but there is no tool to ask about either.
            for name in [
                "memory_save",
                "memory_revise",
                "memory_get",
                "memory_archive",
                "memory_forget",
                "memory_purge",
                "memory_confirm",
            ] {
                assert_eq!(action(&profile, name), Action::Ask, "{tier}: {name}");
            }
        }
    }

    /// ADR 0024 Q6: `ask_user` writes nothing and runs nothing, so every tier allows it and it
    /// never prompts. Removing the rule would make it fall through to the default `ask`.
    #[test]
    fn ask_user_is_allowed_in_every_tier() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "ask_user"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0043 decision 1a: `subagent` writes nothing and runs no workspace command itself;
    /// the child's own tier and approval govern what it does. Every tier allows it, so a
    /// headless parent that cannot answer an `ask` can still use it. Removing the rule would
    /// make it fall through to the default `ask` and be denied without a terminal.
    #[test]
    fn subagent_is_allowed_in_every_tier() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "subagent"), Action::Allow, "{tier}");
        }
    }

    /// The built-in search tools read the workspace and write nothing, so every tier allows
    /// them and they never prompt (they enforce the workspace boundary in `radian-coding`).
    #[test]
    fn the_search_tools_are_allowed_in_every_tier() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "glob"), Action::Allow, "{tier}");
            assert_eq!(action(&profile, "grep"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0025 S3: reading a skill is `allow` in every tier — it is data, not a permission.
    /// Removing the rule would make it fall through to the default `ask`.
    #[test]
    fn skill_is_allowed_in_every_tier() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "skill"), Action::Allow, "{tier}");
        }
    }
}
