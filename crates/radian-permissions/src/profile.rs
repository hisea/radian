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
    Safe,
    WorkspaceWrite,
    FullAccess,
}

impl Preset {
    /// The user-visible tier name. `workspace-write`/`full-access` remain accepted as CLI
    /// aliases, but this is the name the hosts print and the status bar shows (ADR 0016).
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Safe => "safe",
            Preset::WorkspaceWrite => "workspace",
            Preset::FullAccess => "full",
        }
    }

    pub fn sandbox_mode(self) -> SandboxMode {
        match self {
            Preset::Safe => SandboxMode::Safe,
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
#[derive(Debug, Clone)]
pub struct Profile {
    pub preset: Preset,
    pub sandbox: SandboxMode,
    pub approval: ApprovalPolicy,
    pub workspace: PathBuf,
    rules: Vec<Rule>,
}

impl Profile {
    pub fn safe(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::build(Preset::Safe, workspace, |_ws| {
            let mut rules = read_only();
            rules.extend(memory_rules());
            rules.extend(always_allowed());
            rules.extend([
                Rule::new(
                    ToolMatcher::Name("write".into()),
                    ResourceMatcher::Any,
                    Action::Deny,
                )
                .with_reason("safe mode does not modify files"),
                Rule::new(
                    ToolMatcher::Name("patch".into()),
                    ResourceMatcher::Any,
                    Action::Deny,
                )
                .with_reason("safe mode does not modify files"),
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::Any,
                    Action::Deny,
                )
                .with_reason("safe mode has no shell"),
                // A legacy deny for the removed `git_commit` tool name; safe does not write.
                Rule::new(
                    ToolMatcher::Name("git_commit".into()),
                    ResourceMatcher::Any,
                    Action::Deny,
                )
                .with_reason("safe mode does not write the repository"),
                Rule::new(ToolMatcher::Mcp, ResourceMatcher::Any, Action::Deny)
                    .with_reason("safe mode does not start MCP tools"),
            ]);
            rules
        })
    }

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

    pub fn full_access(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::build(Preset::FullAccess, workspace, |ws| {
            let mut rules = read_only();
            rules.extend(memory_rules());
            rules.extend(always_allowed());
            rules.extend(write_under(ws));
            rules.extend([
                // Still ask: full-access is not "run anything silently".
                Rule::new(
                    ToolMatcher::Name("shell".into()),
                    ResourceMatcher::Any,
                    Action::Ask,
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
            rules,
        })
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
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

    /// ADR 0021 D5/D7: `memory_search` is allowed in all three tiers; no memory tool asks.
    #[test]
    fn memory_search_is_allowed_in_every_tier_and_no_memory_tool_asks() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::safe(&ws).unwrap(),
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
            Profile::safe(&ws).unwrap(),
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
            Profile::safe(&ws).unwrap(),
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
            Profile::safe(&ws).unwrap(),
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
            Profile::safe(&ws).unwrap(),
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "skill"), Action::Allow, "{tier}");
        }
    }
}
