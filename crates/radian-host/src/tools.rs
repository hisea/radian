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

use crate::memory::{MemorySession, MemoryTools};
use crate::skills::tools::{SkillSession, SkillTools};
use crate::subagent::{SubagentSession, SubagentTools};

pub struct HostTools {
    coding: Option<CodingTools>,
    memory: Option<MemoryTools>,
    /// `ask_user` (ADR 0024 Q5). Separate from the coding tools: a host with no workspace
    /// still has a human to ask.
    ask: Option<AskTools>,
    /// `skill` (ADR 0025 S3). Read-only, so it is mounted whenever skills were discovered.
    skills: Option<SkillTools>,
    /// `subagent` (ADR 0043 decision 1a). Mounted only in a parent session; `open_session`
    /// never passes it for a session whose `parent` relation is `"subagent"` (depth 1).
    subagent: Option<SubagentTools>,
    mcp: McpToolProvider,
    /// The session's built-in tool allow-list. `None` (the default) mounts every built-in the
    /// host has; `Some` mounts only these names — the others are not
    /// in the model's tool list at all and are not in the `<runtime>` layer (the permission
    /// `deny` the registry still installs is defence in depth beneath that). MCP tools are not
    /// in this universe; they stay governed by the session's configured MCP servers/provider.
    tool_allow: Option<Vec<String>>,
}

impl HostTools {
    pub fn new(
        coding: Option<CodingTools>,
        memory: Option<MemoryTools>,
        mcp: McpToolProvider,
    ) -> Self {
        Self {
            coding,
            memory,
            ask: None,
            skills: None,
            subagent: None,
            mcp,
            tool_allow: None,
        }
    }

    /// Mount only the built-in tools this allow-list names. `None` mounts every built-in the
    /// host has. This is the "not in the list is not
    /// mounted" rule; the permission layer's `deny` (the registry's `with_denied_tools`) still
    /// stands beneath it, so a name that somehow reaches the layer is refused too.
    pub fn with_tool_allow(mut self, allow: Option<Vec<String>>) -> Self {
        self.tool_allow = allow;
        self
    }

    /// Whether a built-in tool name may be mounted under the allow-list.
    fn allows(&self, name: &str) -> bool {
        match &self.tool_allow {
            Some(allow) => allow.iter().any(|allowed| allowed == name),
            None => true,
        }
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

    /// Mounts `subagent` (ADR 0043 decision 1a). A child session never receives this: the
    /// caller (`open_session`) drops the launch parts when the session's `parent` relation is
    /// `"subagent"`, which is the depth-1 rule.
    pub fn with_subagent(mut self, subagent: SubagentTools) -> Self {
        self.subagent = Some(subagent);
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
                "job",
                "read, wait for, or stop a background shell job by id",
            );
            push(
                "git_diff",
                "a diff of the working tree; `stat: true` gives the status and a --stat summary",
            );
        }
        if self.memory.is_some() {
            push(
                "memory_search",
                "search durable memory, or read records in full by id",
            );
            push("memory_save", "save a memory record");
            push("memory_revise", "revise a memory record");
        }
        if self.ask.is_some() {
            push("ask_user", "ask the user a structured question");
        }
        if self.skills.is_some() {
            push("skill", "read one of the discovered skills");
        }
        if self.subagent.is_some() {
            push(
                "subagent",
                "run one task in a fresh blocking child session (`explore` is read-only) and \
                 return its last assistant text",
            );
        }
        // The session allow-list narrows what is even listed in layer 0a; a built-in it
        // does not name is not mounted and must not appear here.
        if self.tool_allow.is_some() {
            out.retain(|(name, _)| self.allows(name));
        }
        out
    }
}

pub struct HostSession {
    coding: Option<CodingSession>,
    memory: Option<MemorySession>,
    ask: Option<AskSession>,
    skills: Option<SkillSession>,
    subagent: Option<SubagentSession>,
    mcp: McpSession,
    specs: Vec<ToolSpec>,
    coding_names: HashSet<String>,
    memory_names: HashSet<String>,
    ask_names: HashSet<String>,
    skill_names: HashSet<String>,
    subagent_names: HashSet<String>,
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
        let subagent = match &self.subagent {
            Some(subagent) => Some(subagent.open_session().await?),
            None => None,
        };

        let mut coding_names = HashSet::new();
        let mut memory_names = HashSet::new();
        let mut ask_names = HashSet::new();
        let mut skill_names = HashSet::new();
        let mut subagent_names = HashSet::new();
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
        if let Some(subagent) = &subagent {
            for spec in subagent.tool_specs() {
                subagent_names.insert(spec.name.clone());
                built_in_specs.push(spec.clone());
            }
        }
        // The session allow-list is a physical unmount. Drop every unlisted built-in spec
        // **and** its dispatch name, so a call to it is an unknown tool, not merely denied. MCP
        // specs are dynamic and are never filtered here (the session's configured MCP servers
        // govern them). The permission-layer `deny` for the same names is kept (defence depth).
        if self.tool_allow.is_some() {
            coding_names.retain(|name| self.allows(name));
            memory_names.retain(|name| self.allows(name));
            ask_names.retain(|name| self.allows(name));
            skill_names.retain(|name| self.allows(name));
            subagent_names.retain(|name| self.allows(name));
            built_in_specs.retain(|spec| self.allows(&spec.name));
        }
        let specs = merge_tool_specs(built_in_specs, mcp.tool_specs().to_vec())?;

        Ok(HostSession {
            coding,
            memory,
            ask,
            skills,
            subagent,
            mcp,
            specs,
            coding_names,
            memory_names,
            ask_names,
            skill_names,
            subagent_names,
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
        if self.skill_names.contains(&call.name) {
            match &self.skills {
                Some(skills) => skills.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no skill session", call.name),
                )),
            }
        } else if self.subagent_names.contains(&call.name) {
            match &self.subagent {
                Some(subagent) => subagent.execute(call, cx).await,
                None => Err(ToolFailure::new(
                    "unknown_tool",
                    format!("`{}` has no subagent session", call.name),
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
            coding.shutdown_outputs();
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
