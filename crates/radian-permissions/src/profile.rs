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
                // ADR 0030 R10: `git_commit` writes the repository; safe does not write.
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

    /// Extends this profile with the ADR 0030 R1 **hard-deny** command list. The
    /// registry calls this on whatever profile the task builder supplied, so no definition
    /// file, project config or edge policy can relax it: `deny` outranks `ask`/`allow`, and
    /// `ApprovalPolicy` never lowers a `deny`. The rules use the conservative `deny` matcher
    /// ([`crate::shell::deny_matches`]), so a segment of a composite command is enough to deny.
    pub fn with_task_hard_deny(mut self) -> Self {
        self.rules.extend(task_hard_deny_rules());
        self
    }

    /// Allows these tool names in **every** tier, with the normal rule priority (ADR 0030 slice
    /// 6): the lead tools `spawn_task`, `wait_task`, … write no workspace file and run no
    /// repository command themselves — the task worktree and its sandbox are the boundary — so
    /// they never prompt. A `deny` rule still wins, and the host's [`crate::ToolGate`] is checked
    /// before the rules, so `/mode general` still refuses them (ADR 0038 decision 1).
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

    /// Denies every tool in `names` (ADR 0030 problem 7 = A). An agent definition's `tools`
    /// allow-list is enforced with this: the host passes the built-ins the list does **not**
    /// name, and `deny` outranks `allow`/`ask`, so a listed tool the tier would not mount stays
    /// unmounted and an unlisted tool cannot run. It only ever adds `deny` rules, so it can
    /// narrow a tier but never widen one (ADR 0030 O2).
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
            .with_reason("not in this agent's tool allow-list (ADR 0030 O11)")
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

/// The memory tools (ADR 0021 D5/D7). Reading is allowed in every tier; writing always asks,
/// in every tier. `--yes` answers the ask like any other, and a `deny` rule would still win.
/// There is deliberately **no** archive/forget/purge/confirm tool (agent writes are limited to
/// save/revise, D5).
fn memory_rules() -> Vec<Rule> {
    [
        ("memory_search", Action::Allow),
        ("memory_get", Action::Allow),
        ("memory_save", Action::Ask),
        ("memory_revise", Action::Ask),
    ]
    .into_iter()
    .map(|(name, action)| Rule::new(ToolMatcher::Name(name.into()), ResourceMatcher::Any, action))
    .collect()
}

/// The read-only built-ins, allowed regardless of path: `radian-coding` enforces the
/// workspace boundary itself for these tools, so the permission layer does not need to.
fn read_only() -> Vec<Rule> {
    ["read", "list", "glob", "grep", "git_status", "git_diff"]
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
/// through the ordinary hook path.
fn always_allowed() -> Vec<Rule> {
    ["ask_user", "skill"]
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

/// The ADR 0030 R1 hard-deny command list, as `deny` rules on the `shell` tool. These
/// are checked before any edge policy and cannot be relaxed. The matcher is conservative over
/// every shell segment ([`crate::shell::deny_matches`]): a `deny` command rule also matches inside
/// a quoted or composite command, so `sh -c 'git push'` is denied too.
pub fn task_hard_deny_rules() -> Vec<Rule> {
    const DENIED: &[&[&str]] = &[
        &["git", "push"],
        // Merge/checkout against the lead workspace or a user branch: the task has its own
        // worktree and never needs another branch's content in it.
        &["git", "merge"],
        &["git", "checkout", "main"],
        &["git", "checkout", "master"],
        // `git checkout -- <path>` / `git checkout .` are the destructive restore forms.
        &["git", "checkout", "--"],
        &["git", "checkout", "."],
        &["git", "reset", "--hard"],
        &["git", "restore"],
        &["git", "stash"],
    ];
    DENIED
        .iter()
        .map(|prefix| {
            Rule::new(
                ToolMatcher::Name("shell".into()),
                ResourceMatcher::CommandPrefix(prefix.iter().map(|w| (*w).to_owned()).collect()),
                Action::Deny,
            )
            .with_reason("a task agent may not run this (ADR 0030 R1)")
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

    /// ADR 0021 D5/D7: `memory_search`/`memory_get` are allowed in all three tiers;
    /// `memory_save`/`memory_revise` ask in all three; no destructive memory tool exists.
    #[test]
    fn the_memory_tools_read_allowed_and_write_asked_in_every_tier() {
        let ws = std::env::temp_dir();
        for profile in [
            Profile::safe(&ws).unwrap(),
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "memory_search"), Action::Allow, "{tier}");
            assert_eq!(action(&profile, "memory_get"), Action::Allow, "{tier}");
            assert_eq!(action(&profile, "memory_save"), Action::Ask, "{tier}");
            assert_eq!(action(&profile, "memory_revise"), Action::Ask, "{tier}");
            // No rule matches a destructive name, so the default `ask` stands — but there is
            // no tool to ask about either.
            for name in [
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
