//! Matching a tool call against the rule set.

use std::path::{Component, Path, PathBuf};

use agent_core::ToolCall;

use crate::shell;

/// The three rule actions. `Deny` beats `Ask` beats `Allow`; no match means `Ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    fn rank(self) -> u8 {
        match self {
            Action::Allow => 0,
            Action::Ask => 1,
            Action::Deny => 2,
        }
    }
}

/// Which tool a rule applies to. `Mcp` matches the `server__tool` alias form
/// (`crates/agent-extensions/src/alias.rs`); it is an approximation — a tool whose name
/// contains `__` for another reason would also match, and a namer that changes the alias
/// shape would not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolMatcher {
    Any,
    Name(String),
    Prefix(String),
    Mcp,
}

impl ToolMatcher {
    pub fn matches(&self, tool: &str) -> bool {
        match self {
            ToolMatcher::Any => true,
            ToolMatcher::Name(name) => name == tool,
            ToolMatcher::Prefix(prefix) => tool.starts_with(prefix.as_str()),
            ToolMatcher::Mcp => tool.contains("__"),
        }
    }
}

/// What part of a call a rule looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceMatcher {
    Any,
    /// A file path, matched after resolution against the canonical prefix.
    PathUnder(PathBuf),
    /// A `.git` executable entry under one discovered git directory (ADR 0038 decision 3):
    /// matches when the resolved path is `config`, `config.worktree`, `hooks/...`, a
    /// submodule's `config` or `hooks/...`, or a linked worktree's `gitdir`/`commondir`/
    /// `config.worktree` under the directory (the shared list in `radian_sandbox::git_exec`).
    /// The whole git directory is **not** matched — `.git/info/exclude` stays writable — and
    /// a worktree pointer **file** is not matched either (deny it with [`ResourceMatcher::PathUnder`]
    /// on the file itself). Matching is by path components, so `config` never spills onto
    /// `config-other`.
    GitExec(PathBuf),
    /// A simple shell command whose tokens start with this prefix.
    CommandPrefix(Vec<String>),
    /// An exact MCP tool alias (or any tool name when the call has no path/command).
    Name(String),
}

/// One rule. Rules are a flat list; [`evaluate`] combines their actions by priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub tool: ToolMatcher,
    pub resource: ResourceMatcher,
    pub action: Action,
    pub reason: Option<String>,
}

impl Rule {
    pub fn new(tool: ToolMatcher, resource: ResourceMatcher, action: Action) -> Self {
        Self {
            tool,
            resource,
            action,
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// The part of a call that a resource rule matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Path(String),
    Command(String),
    Name(String),
}

impl Resource {
    /// A display form for the approval request and audit record.
    pub fn display(&self) -> &str {
        match self {
            Resource::Path(p) | Resource::Command(p) | Resource::Name(p) => p,
        }
    }
}

/// The name a call is judged under. Built-in tools renamed in ADR 0043 decision 3 keep
/// their old names as aliases for one version; the alias is decided by the **new**
/// tool's rule (no bypass), so a rule, a session grant or the approver can never single out the
/// old name. A config rule written against an old name therefore no longer matches: the alias is
/// checked under the new name only.
pub fn canonical_tool_name(name: &str) -> &str {
    match name {
        "git_status" => "git_diff",
        "list" => "read",
        "job_output" | "job_list" | "job_kill" | "shell_output" => "job",
        other => other,
    }
}

/// Extracts the resource from a call. `command` wins over `path`; a call with neither is
/// matched by its tool name (MCP and any other parameterless tool).
pub fn resource_of(call: &ToolCall) -> Resource {
    if let Some(command) = call.arguments.get("command").and_then(|v| v.as_str()) {
        Resource::Command(command.to_owned())
    } else if let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) {
        Resource::Path(path.to_owned())
    } else {
        Resource::Name(call.name.clone())
    }
}

/// The outcome of matching: the winning action and the reason of the rule that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluated {
    pub action: Action,
    pub reason: Option<String>,
}

impl Evaluated {
    pub fn ask() -> Self {
        Self {
            action: Action::Ask,
            reason: None,
        }
    }
}

/// Combines every matching rule by priority: `Deny` > `Ask` > `Allow`; no match is `Ask`.
///
/// `allow`/`ask` command rules only match a simple command (see [`crate::shell`]);
/// an `allow` rule additionally never matches a command that starts with a
/// runs-its-arguments program ([`crate::shell::WRAPPER_PROGRAMS`]); `deny`
/// command rules are matched conservatively over every segment. Path rules match the
/// **resolved** path, so `..` and symlinks cannot change which rule they hit.
pub fn evaluate(rules: &[Rule], workspace: &Path, call: &ToolCall) -> Evaluated {
    let resource = resource_of(call);
    let mut best: Option<&Rule> = None;
    for rule in rules {
        if !rule.tool.matches(&call.name) {
            continue;
        }
        let matched = match (&rule.resource, &resource) {
            (ResourceMatcher::Any, _) => true,
            (ResourceMatcher::PathUnder(prefix), Resource::Path(raw)) => {
                resolve(workspace, raw).starts_with(prefix)
            }
            (ResourceMatcher::GitExec(git_dir), Resource::Path(raw)) => {
                radian_sandbox::is_git_exec_entry(git_dir, &resolve(workspace, raw))
            }
            (ResourceMatcher::CommandPrefix(prefix), Resource::Command(command)) => {
                if rule.action == Action::Deny {
                    shell::deny_matches(command, prefix)
                } else if rule.action == Action::Allow && shell::starts_with_wrapper(command) {
                    // A runs-its-arguments program (`xargs rm`, `sh -c …`) only shows the
                    // wrapper's own tokens; a prefix rule cannot see what it will run, so
                    // it never allows it. `deny` still runs first, above.
                    false
                } else {
                    shell::simple_prefix_matches(command, prefix)
                }
            }
            (ResourceMatcher::Name(want), Resource::Name(name)) => want == name,
            _ => false,
        };
        if matched && best.is_none_or(|current| rule.action.rank() > current.action.rank()) {
            best = Some(rule);
        }
    }
    match best {
        Some(rule) => Evaluated {
            action: rule.action,
            reason: rule.reason.clone(),
        },
        None => Evaluated::ask(),
    }
}

/// Resolves a call's path for matching: relative paths are joined to the workspace, symlinks
/// in the existing part are resolved, and `.`/`..` are removed lexically. The result never
/// contains `..`, so a `PathUnder` prefix cannot be escaped by it or by a symlink.
pub(crate) fn resolve(workspace: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    };
    let resolved = canonicalize_existing_prefix(&joined).unwrap_or(joined);
    lexical_normalize(&resolved)
}

fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut out = canonical;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let name = existing.file_name()?.to_os_string();
        tail.push(name);
        if !existing.pop() {
            return None;
        }
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}
