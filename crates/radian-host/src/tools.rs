//! The combined tool provider: the built-in coding tools, the agent memory tools and the MCP
//! servers, behind one `ToolProvider` so `Agent` still has a single provider type.
//!
//! Built-in names (coding and memory) are checked first and may not collide with each other or
//! with an MCP tool name; a collision fails `open_session` rather than silently shadowing one
//! of them.

use std::collections::HashSet;

use agent_core::{
    ToolCall, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use agent_extensions::{McpSession, McpToolProvider};
use radian_coding::{AskSession, AskTools, CodingSession, CodingTools};

use crate::git_commit::{GitCommitTools, run as run_git_commit};
use crate::lead_tools::{LeadSession, LeadTools};
use crate::memory::{MemorySession, MemoryTools};
use crate::mermaid::{MermaidCheckSession, MermaidCheckTools};
use crate::skills::tools::{SkillSession, SkillTools};

pub struct HostTools {
    coding: Option<CodingTools>,
    memory: Option<MemoryTools>,
    /// `ask_user` (ADR 0024 Q5). Separate from the coding tools: a host with no workspace
    /// still has a human to ask.
    ask: Option<AskTools>,
    /// `skill` (ADR 0025 S3). Read-only, so it is mounted whenever skills were discovered.
    skills: Option<SkillTools>,
    /// `mermaid_check` (ADR 0028 R8 / ADR 0025). Read-only and always mounted.
    mermaid: MermaidCheckTools,
    /// `git_commit` (ADR 0030 R10). Runs in the host, outside the sandbox. `None` for a host
    /// with no workspace/journal (the CLI's own tool tests).
    git_commit: Option<GitCommitTools>,
    /// The lead's task tools (ADR 0030 slice 6). `None` for a task agent (depth limit) and for
    /// a host that supplies no task builder.
    lead: Option<LeadTools>,
    mcp: McpToolProvider,
}

impl HostTools {
    pub fn new(
        coding: Option<CodingTools>,
        memory: Option<MemoryTools>,
        mcp: McpToolProvider,
        git_commit: Option<GitCommitTools>,
    ) -> Self {
        Self {
            coding,
            memory,
            ask: None,
            skills: None,
            mermaid: MermaidCheckTools::new(),
            git_commit,
            lead: None,
            mcp,
        }
    }

    /// Mounts the lead's task tools from the start (ADR 0030 item 5). The permission
    /// layer decides whether they run: a mode gate denies them outside `lead` (ADR 0038
    /// decision 1). Never mounted for a task agent.
    pub fn with_lead(mut self, lead: LeadTools) -> Self {
        self.lead = Some(lead);
        self
    }

    /// Mounts `ask_user`. The host calls this only when a human is there to answer; without it
    /// the model's tool list has no `ask_user` at all.
    pub fn with_ask(mut self, ask: AskTools) -> Self {
        self.ask = Some(ask);
        self
    }

    /// Mounts the `skill` tool (ADR 0025 S3). Without a discovered skill the model's tool list
    /// has no `skill` at all, so a host with no skills does not advertise it.
    pub fn with_skills(mut self, skills: SkillTools) -> Self {
        self.skills = Some(skills);
        self
    }

    /// The mounted built-in tools as `(name, one-line purpose)`, for the system prompt's
    /// `<runtime>` layer (ADR 0036 D1's 0a). The MCP tools are dynamic (they depend on the
    /// server), so they are named by server in the same layer, not listed here.
    pub fn builtin_tools(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut push = |name: &str, purpose: &str| {
            out.push((name.to_owned(), purpose.to_owned()));
        };
        if self.coding.is_some() {
            push(
                "read",
                "read a file; lines are numbered, with offset and limit",
            );
            push(
                "patch",
                "replace text in a file (layered match; replace_all for many)",
            );
            push("write", "create or overwrite a file");
            push("list", "list a directory");
            push("glob", "find files whose path matches a glob");
            push("grep", "search file contents with a regex");
            push(
                "shell",
                "run a command in the workspace; one that runs past the auto-background \
                 threshold is moved to the background automatically (`background: true` starts \
                 it there at once)",
            );
            push(
                "job_output",
                "read a background job's new output (wait: true waits for the end)",
            );
            push("job_list", "list this session's background jobs");
            push("job_kill", "stop a background job's process group");
            push("git_status", "the working tree status");
            push("git_diff", "a diff of the working tree");
        }
        if self.memory.is_some() {
            push("memory_search", "search durable memory");
            push("memory_get", "read one memory record");
            push("memory_save", "save a memory record");
            push("memory_revise", "revise a memory record");
        }
        if self.ask.is_some() {
            push("ask_user", "ask the user a structured question");
        }
        if self.skills.is_some() {
            push("skill", "read one of the discovered skills");
        }
        push("mermaid_check", "validate a Mermaid diagram");
        if self.git_commit.is_some() {
            push(
                "git_commit",
                "commit inside the workspace from the host; needed when the sandbox keeps \
                 `.git` read-only (task agents, Linux)",
            );
        }
        if self.lead.is_some() {
            for (name, purpose) in crate::lead_tools::specs()
                .iter()
                .map(|spec| (spec.name.clone(), first_line(&spec.description)))
            {
                out.push((name, purpose));
            }
        }
        out
    }
}

/// The first sentence of a tool description, for the `<runtime>` layer's one-line list.
fn first_line(description: &str) -> String {
    description
        .split(". ")
        .next()
        .unwrap_or(description)
        .trim()
        .to_owned()
}

pub struct HostSession {
    coding: Option<CodingSession>,
    memory: Option<MemorySession>,
    ask: Option<AskSession>,
    skills: Option<SkillSession>,
    mermaid: MermaidCheckSession,
    git_commit: Option<GitCommitTools>,
    lead: Option<LeadSession>,
    mcp: McpSession,
    specs: Vec<ToolSpec>,
    coding_names: HashSet<String>,
    memory_names: HashSet<String>,
    ask_names: HashSet<String>,
    skill_names: HashSet<String>,
    mermaid_names: HashSet<String>,
    git_commit_names: HashSet<String>,
    lead_names: HashSet<String>,
}

impl ToolProvider for HostTools {
    type Session = HostSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        // Open MCP first: if it fails, no built-in state needs unwinding.
        let mcp = self.mcp.open_session().await?;
        let coding = match &self.coding {
            Some(coding) => Some(coding.open_session().await?),
            None => None,
        };
        let memory = match &self.memory {
            Some(memory) => Some(memory.open_session().await?),
            None => None,
        };
        let ask = match &self.ask {
            Some(ask) => Some(ask.open_session().await?),
            None => None,
        };
        let skills = match &self.skills {
            Some(skills) => Some(skills.open_session().await?),
            None => None,
        };
        let mermaid = self.mermaid.open_session().await?;
        let git_commit = self.git_commit.clone();
        let lead = match &self.lead {
            Some(lead) => Some(lead.open_session().await?),
            None => None,
        };

        let mut coding_names = HashSet::new();
        let mut memory_names = HashSet::new();
        let mut ask_names = HashSet::new();
        let mut skill_names = HashSet::new();
        let mut mermaid_names = HashSet::new();
        let mut git_commit_names = HashSet::new();
        let mut lead_names = HashSet::new();
        let mut built_in_specs: Vec<ToolSpec> = Vec::new();
        if let Some(coding) = &coding {
            for spec in coding.tool_specs() {
                coding_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        if let Some(memory) = &memory {
            for spec in memory.tool_specs() {
                memory_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        if let Some(ask) = &ask {
            for spec in ask.tool_specs() {
                ask_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        if let Some(skills) = &skills {
            for spec in skills.tool_specs() {
                skill_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        for spec in mermaid.tool_specs() {
            mermaid_names.insert(spec.name.clone());
            built_in_specs.push(spec.clone());
        }
        if git_commit.is_some() {
            let git_commit_spec = crate::git_commit::spec();
            git_commit_names.insert(git_commit_spec.name.clone());
            built_in_specs.push(git_commit_spec);
        }
        if let Some(lead) = &lead {
            for spec in lead.tool_specs() {
                lead_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        let specs = merge_tool_specs(built_in_specs, mcp.tool_specs().to_vec())?;

        Ok(HostSession {
            coding,
            memory,
            ask,
            skills,
            mermaid,
            git_commit,
            lead,
            mcp,
            specs,
            coding_names,
            memory_names,
            ask_names,
            skill_names,
            mermaid_names,
            git_commit_names,
            lead_names,
        })
    }
}

impl ToolSession for HostSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if self.git_commit_names.contains(&call.name) {
            // The host tool needs the workspace, the journal and the branch rules, so it runs
            // through its own entry point rather than the plain `ToolSession` trait.
            match &self.git_commit {
                Some(git_commit) => run_git_commit(git_commit, call).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no host session", call.name),
                )),
            }
        } else if self.lead_names.contains(&call.name) {
            match &self.lead {
                Some(lead) => crate::lead_tools::run(lead, call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no lead session", call.name),
                )),
            }
        } else if self.mermaid_names.contains(&call.name) {
            self.mermaid.execute(call, cx).await
        } else if self.skill_names.contains(&call.name) {
            match &self.skills {
                Some(skills) => skills.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no skill session", call.name),
                )),
            }
        } else if self.ask_names.contains(&call.name) {
            match &self.ask {
                Some(ask) => ask.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no question session", call.name),
                )),
            }
        } else if self.memory_names.contains(&call.name) {
            match &self.memory {
                Some(memory) => memory.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no memory session", call.name),
                )),
            }
        } else if self.coding_names.contains(&call.name) {
            match &self.coding {
                Some(coding) => coding.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no built-in session", call.name),
                )),
            }
        } else {
            self.mcp.execute(call, cx).await
        }
    }

    async fn close(&mut self) -> Result<(), ToolError> {
        // Session end (ADR 0037 decision 6): kill every running background job's process
        // group. The registry also kills on drop (the panic path), but a normal shutdown
        // names it here.
        if let Some(coding) = &self.coding {
            coding.shutdown_jobs();
        }
        self.mcp.close().await
    }
}

/// Merges the built-in specs (coding then memory) with the MCP specs, refusing a name that is
/// defined twice — including two built-ins.
///
/// Split out and public so the rule is testable on its own: with the current alias scheme
/// (`server__tool`) a real MCP server cannot produce a built-in name, so a silent override
/// would be the only way this could ever go wrong — and an error is the safe end of that.
pub fn merge_tool_specs(
    built_in: Vec<ToolSpec>,
    mcp: Vec<ToolSpec>,
) -> Result<Vec<ToolSpec>, ToolError> {
    let mut specs: Vec<ToolSpec> = Vec::with_capacity(built_in.len() + mcp.len());
    for spec in built_in {
        if specs.iter().any(|existing| existing.name == spec.name) {
            return Err(ToolError(format!(
                "duplicate built-in tool name `{}`: two built-in providers define it",
                spec.name
            )));
        }
        specs.push(spec);
    }
    for spec in mcp {
        if specs.iter().any(|existing| existing.name == spec.name) {
            return Err(ToolError(format!(
                "duplicate tool name `{}`: the built-in tools and an MCP server both define it",
                spec.name
            )));
        }
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolConcurrency;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Exclusive,
        }
    }

    #[test]
    fn a_duplicate_tool_name_is_refused_not_shadowed() {
        let error = merge_tool_specs(vec![spec("read")], vec![spec("read")]).unwrap_err();
        assert!(
            error.0.contains("duplicate tool name `read`"),
            "{}",
            error.0
        );
    }

    #[test]
    fn two_built_in_providers_cannot_define_the_same_name() {
        let error = merge_tool_specs(vec![spec("read"), spec("read")], vec![]).unwrap_err();
        assert!(error.0.contains("two built-in providers"), "{}", error.0);
    }

    #[test]
    fn distinct_names_merge_in_order() {
        let merged = merge_tool_specs(vec![spec("read")], vec![spec("fx__echo")]).unwrap();
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["read", "fx__echo"]);
    }
}
