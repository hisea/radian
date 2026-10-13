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
    pub fn as_str(self) -> &'static str {
        match self {
            Preset::Safe => "safe",
            Preset::WorkspaceWrite => "workspace-write",
            Preset::FullAccess => "full-access",
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
                Rule::new(ToolMatcher::Mcp, ResourceMatcher::Any, Action::Deny)
                    .with_reason("safe mode does not start MCP tools"),
            ]);
            rules
        })
    }

    pub fn workspace_write(workspace: impl AsRef<Path>) -> Result<Self, ProfileError> {
        Self::build(Preset::WorkspaceWrite, workspace, |ws| {
            let mut rules = read_only();
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

    /// Replaces the approval policy (used by `custom` callers and tests).
    pub fn with_approval(mut self, approval: ApprovalPolicy) -> Self {
        self.approval = approval;
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

/// The read-only built-ins, allowed regardless of path: `radian-coding` enforces the
/// workspace boundary itself for these tools, so the permission layer does not need to.
fn read_only() -> Vec<Rule> {
    ["read", "list", "git_status", "git_diff"]
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
