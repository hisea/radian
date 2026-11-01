//! Named permission presets: the three layers of ADR 0012 as one profile.

use std::path::{Path, PathBuf};

use radian_sandbox::{Sandbox, SandboxMode};

use crate::rule::{Action, ResourceMatcher, Rule, ToolMatcher};

/// The `--profile` tier names, in documentation order. The parsers and the manual's drift
/// test both read this list.
pub const TIER_NAMES: &[&str] = &["workspace", "full"];
/// The host-maintained memory file the model may never write (ADR 0021 D3).
pub const SESSIONS_INDEX_FILE: &str = "SESSIONS.md";
/// The `--profile` aliases, accepted but hidden from `--help`.
pub const TIER_ALIASES: &[&str] = &["workspace-write", "full-access"];
///
/// The `--approval` / `[permissions] approval` values, in documentation order. The parser
/// and the manual's drift test both read this list.
pub const APPROVAL_POLICIES: &[&str] = &["on-ask", "never", "always", "auto"];

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
        if !APPROVAL_POLICIES.contains(&name) {
            return None;
        }
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

/// Why a profile could not be built (a workspace root that does not resolve, or a
/// sandbox that does not enforce anything).
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("{path}: cannot use as the permission workspace root: {source}")]
    Workspace {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: the permission workspace root is not a directory")]
    NotADirectory(PathBuf),
    /// A sandboxed profile needs an enforced sandbox: a probed local backend or
    /// `inherited`, never the transparent `FullAccess` backend.
    #[error(
        "a sandboxed `workspace` profile needs an enforced sandbox (a probed local backend or `inherited`), not a transparent one"
    )]
    SandboxNotEnforced,
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
            rules.extend(git_exec_denies(ws));
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
    /// (a `deny` rule still wins first, and MCP stays `ask`). The proof is the `sandbox`
    /// argument itself — a probed local backend, or `inherited` — so passing a transparent
    /// sandbox (`FullAccess`, which enforces nothing) is refused instead of building the
    /// allow-shell profile. Using the profile without an enforced sandbox is the mutation
    /// `workspace_sandboxed_shell_is_allowed_only_with_a_sandbox` catches.
    pub fn workspace_write_sandboxed(
        workspace: impl AsRef<Path>,
        sandbox: &Sandbox,
    ) -> Result<Self, ProfileError> {
        if !sandbox.mode().is_enforced() {
            return Err(ProfileError::SandboxNotEnforced);
        }
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

    /// Marks the profile as sandboxed from an enforced [`Sandbox`] (hosts that add their
    /// own rules with [`Profile::custom`] on top of a sandboxed session). Only `Some` with
    /// an enforced mode (`is_enforced`: a probed local backend, or `inherited`) sets the
    /// flag; `None` and a transparent sandbox leave it unsandboxed. It only ever narrows
    /// the question, never the boundary: approval still cannot widen the sandbox.
    pub fn with_sandbox(mut self, sandbox: Option<&Sandbox>) -> Self {
        self.sandboxed = sandbox.is_some_and(|sandbox| sandbox.mode().is_enforced());
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

    /// Adds the memory write rule (ADR 0012; ADR 0021 invariant 5 and D7): `write` and
    /// `patch` are `allow` under `<memory root>/global/` and
    /// `<memory root>/projects/<ProjectId>/`, in every tier. A profile built without a
    /// memory root (memory off) has no memory rule.
    ///
    /// The prefixes are resolved like a call's path (symlinks and `..` removed), so a path
    /// that escapes the memory directory through `..` or a symlink does not match. The
    /// project scope needs a project-id directory between `projects/` and the record: a
    /// file directly in `<memory root>/projects/` does not match. The registry
    /// (`<memory root>/projects.jsonl`) and any other directory under the root are not
    /// covered either: only the two scope directories are.
    pub fn with_memory_root(mut self, memory_root: impl AsRef<Path>) -> Self {
        let root = crate::rule::resolve_path(memory_root.as_ref());
        // `SESSIONS.md` is host-maintained and not a memory record (ADR 0021 D3). It is
        // deliberately outside this allow rule, so the tier's default decides and the file
        // tools' own refusal (`SESSIONS.md is maintained by radian`) reads the same in both.
        let index = SESSIONS_INDEX_FILE.to_owned();
        for tool in ["write", "patch"] {
            self.rules.push(Rule::new(
                ToolMatcher::Name(tool.into()),
                ResourceMatcher::PathUnderExceptName(root.join("global"), index.clone()),
                Action::Allow,
            ));
            self.rules.push(Rule::new(
                ToolMatcher::Name(tool.into()),
                ResourceMatcher::PathUnderNestedExceptName(root.join("projects"), index.clone()),
                Action::Allow,
            ));
        }
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

/// ADR 0021 D7: `memory_search` is the model's only memory tool, so every tier allows it and
/// it never prompts. It is still mounted when the injection budget is exceeded
/// (`radian-host`), so removing the rule would turn every call into an `ask` — a prompt in
/// the TUI and a denial in a headless turn. There is no memory write tool: the model writes
/// records with `write` and `patch` (see [`Profile::with_memory_root`]). A `deny` rule still
/// wins.
fn memory_rules() -> Vec<Rule> {
    ["memory_search"]
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

/// The read-only built-ins, allowed regardless of path: `radian-coding` enforces the
/// workspace boundary itself for these tools, so the permission layer does not need to.
fn read_only() -> Vec<Rule> {
    ["read", "glob", "grep"]
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
/// through the ordinary hook path. `subagent` (ADR 0043 decision 2) is here for a different
/// reason: the tool itself writes no file and runs no workspace command; it starts a child
/// session whose own tier and approval decide everything it may do. A parent prompt would only
/// add a second gate that a headless parent cannot answer. `goal` (ADR 0047) is here for the
/// first reason: it only advances the session's own goal state and journals it — no workspace
/// file, no command — so prompting for it would only stall the continuation it serves. `job`
/// (ADR 0037 decision 8) only reads, waits on or stops the session's own jobs: the command was
/// approved when it started, and reading its output later adds no rights, so a second question
/// would only stall a headless turn. Mounting is separate: a tier or a session that does not
/// offer `job` never reaches this rule, and a `deny` rule still outranks it.
fn always_allowed() -> Vec<Rule> {
    ["ask_user", "skill", "subagent", "goal", "job"]
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

/// ADR 0038 decision 3: in the `workspace` tier the file tools deny the `.git` entries git
/// can execute — the same shared list the sandbox enforces (`radian_sandbox::git_exec`).
/// `write`/`patch` never enter the sandbox, so without these rules the model could write
/// `.git/hooks/pre-commit` and have it run at the next commit. `full` has no such rules by
/// design (it allows everything). A worktree pointer file is denied whole: rewriting it
/// repoints git at a gitdir the model controls.
fn git_exec_denies(workspace: &Path) -> Vec<Rule> {
    const REASON: &str = "denied: in the workspace tier, .git entries git can execute (config, hooks) are read-only; writing them would run code outside the sandbox";
    let mut rules = Vec::new();
    for git_path in radian_sandbox::discover_git_metadata(workspace) {
        for tool in ["write", "patch"] {
            let resource = if git_path.is_dir() {
                ResourceMatcher::GitExec(git_path.clone())
            } else {
                ResourceMatcher::PathUnder(git_path.clone())
            };
            rules.push(
                Rule::new(ToolMatcher::Name(tool.into()), resource, Action::Deny)
                    .with_reason(REASON),
            );
        }
    }
    rules
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
    use radian_core::ToolCall;
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

    /// The tier's answer for a call carrying `path` (the shape `write`/`patch` use).
    fn action_at(profile: &Profile, name: &str, path: &Path) -> Action {
        let mut tool_call = call(name);
        tool_call.arguments = json!({ "path": path.to_string_lossy() });
        crate::rule::evaluate(profile.rules(), &profile.workspace, &tool_call).action
    }

    /// The three profiles that carry a memory root: `workspace`, the sandboxed `workspace`
    /// and `full`. `Sandbox::inherited` is enforced without probing, so this is deterministic.
    fn memory_profiles(ws: &TestDir, root: &TestDir) -> Vec<(&'static str, Profile)> {
        let sandbox = radian_sandbox::Sandbox::inherited(ws);
        [
            ("workspace", Profile::workspace_write(ws).unwrap()),
            (
                "sandboxed workspace",
                Profile::workspace_write_sandboxed(ws, &sandbox).unwrap(),
            ),
            ("full", Profile::full_access(ws).unwrap()),
        ]
        .into_iter()
        .map(|(tier, profile)| (tier, profile.with_memory_root(root)))
        .collect()
    }

    /// A fresh empty directory, removed on drop. `workspace_write` discovers git metadata
    /// under the workspace, so these tests use an isolated dir rather than the shared temp
    /// dir itself (which can hold near a million entries on a lived-in machine and make
    /// each profile build walk all of them).
    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-profile-test-{}-{label}-{n}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(std::fs::canonicalize(&dir).unwrap())
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl AsRef<Path> for TestDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    /// Edge case 1 (ADR 0012; ADR 0021 invariant 5/D7): a record file under `global/` is
    /// writable in `workspace`, the sandboxed `workspace` and `full`. The mutation that adds
    /// the rule to `full` only makes the two `workspace` halves fail.
    #[test]
    fn write_to_a_global_memory_file_is_allowed_in_every_tier() {
        let ws = TestDir::new("memory-global-ws");
        let root = TestDir::new("memory-global-root");
        std::fs::create_dir_all(root.0.join("global")).unwrap();
        let path = root.0.join("global/x.md");
        for (tier, profile) in memory_profiles(&ws, &root) {
            assert_eq!(action_at(&profile, "write", &path), Action::Allow, "{tier}");
        }
    }

    /// Edge case 2: a project record (`patch`, and `write` for symmetry) under
    /// `projects/<ProjectId>/` is allowed in every tier.
    #[test]
    fn write_and_patch_to_a_project_memory_file_are_allowed_in_every_tier() {
        let ws = TestDir::new("memory-project-ws");
        let root = TestDir::new("memory-project-root");
        std::fs::create_dir_all(root.0.join("projects/P")).unwrap();
        let path = root.0.join("projects/P/x.md");
        for tool in ["write", "patch"] {
            for (tier, profile) in memory_profiles(&ws, &root) {
                assert_eq!(
                    action_at(&profile, tool, &path),
                    Action::Allow,
                    "{tier}: {tool}"
                );
            }
        }
    }

    /// ADR 0021 D3: `SESSIONS.md` is host-maintained and outside the memory allow rule in
    /// both scope directories, so every tier falls to its default.
    #[test]
    fn a_write_to_the_session_index_is_not_allowed_by_the_memory_rule() {
        let ws = TestDir::new("memory-sessions-ws");
        let root = TestDir::new("memory-sessions-root");
        std::fs::create_dir_all(root.0.join("global")).unwrap();
        std::fs::create_dir_all(root.0.join("projects/P")).unwrap();
        for path in [
            root.0.join("global").join(SESSIONS_INDEX_FILE),
            root.0.join("projects/P").join(SESSIONS_INDEX_FILE),
        ] {
            for tool in ["write", "patch"] {
                for (tier, profile) in memory_profiles(&ws, &root) {
                    assert_eq!(
                        action_at(&profile, tool, &path),
                        Action::Ask,
                        "{tier}: {tool}: {}",
                        path.display()
                    );
                }
            }
        }
    }

    /// Edge case 3: only the two scope directories are covered. The registry
    /// (`projects.jsonl`) and a sibling directory under the root fall to the tier's default.
    /// The mutation that matches any path under `<root>` makes this fail.
    #[test]
    fn a_memory_write_outside_the_scope_directories_is_not_matched() {
        let ws = TestDir::new("memory-registry-ws");
        let root = TestDir::new("memory-registry-root");
        std::fs::create_dir_all(root.0.join("other")).unwrap();
        for path in [root.0.join("projects.jsonl"), root.0.join("other/x.md")] {
            for (tier, profile) in memory_profiles(&ws, &root) {
                assert_eq!(
                    action_at(&profile, "write", &path),
                    Action::Ask,
                    "{tier}: {}",
                    path.display()
                );
            }
        }
    }

    /// Edge case 4: `..` cannot move a call into the memory scope. Matching the raw path
    /// would see `<root>/global/..` and allow it; the resolved path does not.
    #[test]
    fn a_memory_write_that_escapes_with_dotdot_is_not_matched() {
        let ws = TestDir::new("memory-dotdot-ws");
        let root = TestDir::new("memory-dotdot-root");
        std::fs::create_dir_all(root.0.join("global")).unwrap();
        let path = root.0.join("global/../../etc/x");
        for (tier, profile) in memory_profiles(&ws, &root) {
            assert_eq!(action_at(&profile, "write", &path), Action::Ask, "{tier}");
        }
    }

    /// Edge case 5: a symlink inside `global/` that points outside the memory store does not
    /// match (the rule matches the canonical path). Removing the canonicalisation from
    /// `rule::resolve` makes this fail.
    #[test]
    fn a_symlink_out_of_the_memory_store_is_not_matched() {
        let ws = TestDir::new("memory-symlink-ws");
        let root = TestDir::new("memory-symlink-root");
        let outside = TestDir::new("memory-symlink-outside");
        std::fs::create_dir_all(root.0.join("global")).unwrap();
        let target = outside.0.join("x.md");
        std::fs::write(&target, "outside\n").unwrap();
        std::os::unix::fs::symlink(&target, root.0.join("global/link.md")).unwrap();
        let link = root.0.join("global/link.md");
        for (tier, profile) in memory_profiles(&ws, &root) {
            assert_eq!(action_at(&profile, "write", &link), Action::Ask, "{tier}");
        }
    }

    /// Edge case 9: the project scope needs a project-id directory. A file written directly
    /// in `<root>/projects/` is not a record, so the tier's default stands. The mutation that
    /// matches the project scope with the plain `PathUnder` matcher makes this fail.
    #[test]
    fn a_memory_write_directly_in_the_projects_directory_is_not_matched() {
        let ws = TestDir::new("memory-projects-file-ws");
        let root = TestDir::new("memory-projects-file-root");
        std::fs::create_dir_all(root.0.join("projects")).unwrap();
        let path = root.0.join("projects/x.md");
        for (tier, profile) in memory_profiles(&ws, &root) {
            assert_eq!(action_at(&profile, "write", &path), Action::Ask, "{tier}");
        }
    }

    /// ADR 0021 D7: `memory_search` is still mounted, so every tier must allow it; the
    /// tier default would prompt in the TUI and deny in a headless turn. No memory write
    /// tool exists — those names have no rule, so they stay unknown and fall to `ask`.
    #[test]
    fn memory_search_is_allowed_in_every_tier_and_no_memory_write_tool_asks() {
        let ws = TestDir::new("memory-search-ws");
        let root = TestDir::new("memory-search-root");
        for (tier, profile) in memory_profiles(&ws, &root) {
            assert_eq!(action(&profile, "memory_search"), Action::Allow, "{tier}");
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

    /// Edge case 8: a profile built without a memory root (memory off) adds no memory rule;
    /// every other rule is unchanged, so a write under the root asks and the old rules still
    /// answer.
    #[test]
    fn a_profile_without_a_memory_root_has_no_memory_rule() {
        let ws = TestDir::new("memory-off-ws");
        let root = TestDir::new("memory-off-root");
        std::fs::create_dir_all(root.0.join("global")).unwrap();
        let memory_path = root.0.join("global/x.md");
        let workspace_path = ws.0.join("x.md");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(
                action_at(&profile, "write", &memory_path),
                Action::Ask,
                "{tier}: a memory path is not special without a memory root"
            );
            assert_eq!(
                action_at(&profile, "write", &workspace_path),
                Action::Allow,
                "{tier}: the workspace rule is unchanged"
            );
            assert_eq!(action(&profile, "ask_user"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0024 Q6: `ask_user` writes nothing and runs nothing, so every tier allows it and it
    /// never prompts. Removing the rule would make it fall through to the default `ask`.
    #[test]
    fn ask_user_is_allowed_in_every_tier() {
        let ws = TestDir::new("ask-user");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "ask_user"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0047: `goal` only advances the session's own goal state, so every tier allows it
    /// and it never prompts. Removing the rule would make it fall through to the default
    /// `ask` and stall the continuation it serves.
    #[test]
    fn goal_is_allowed_in_every_tier() {
        let ws = TestDir::new("goal");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "goal"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0043 decision 2: `subagent` writes nothing and runs no workspace command itself;
    /// the child's own tier and approval govern what it does. Every tier allows it, so a
    /// headless parent that cannot answer an `ask` can still use it. Removing the rule would
    /// make it fall through to the default `ask` and be denied without a terminal.
    #[test]
    fn subagent_is_allowed_in_every_tier() {
        let ws = TestDir::new("subagent");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "subagent"), Action::Allow, "{tier}");
        }
    }

    /// ADR 0037 decision 8: `job` only reads, waits on or stops the session's own jobs, so
    /// every tier allows it and it never prompts. Removing the rule would make it fall through
    /// to the default `ask` (and, under `approval = "never"`, to a deny).
    #[test]
    fn job_is_allowed_in_every_tier() {
        let ws = TestDir::new("job");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "job"), Action::Allow, "{tier}");
        }
    }

    /// The built-in search tools read the workspace and write nothing, so every tier allows
    /// them and they never prompt (they enforce the workspace boundary in `radian-coding`).
    #[test]
    fn the_search_tools_are_allowed_in_every_tier() {
        let ws = TestDir::new("search");
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
        let ws = TestDir::new("skill");
        for profile in [
            Profile::workspace_write(&ws).unwrap(),
            Profile::full_access(&ws).unwrap(),
        ] {
            let tier = profile.preset.as_str();
            assert_eq!(action(&profile, "skill"), Action::Allow, "{tier}");
        }
    }
}
