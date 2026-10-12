use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};

use crate::workspace::{WorkspaceError, WorkspaceRoot};
use crate::{fail, list, patch, read, write};

/// Resource bounds, all overridable with [`CodingTools::with_limits`]. Defaults are chosen
/// so a single tool call cannot blow up the model context or the session file.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Hard cap for one `read`; a larger `max_bytes` is refused.
    pub max_read_bytes: usize,
    /// `read`'s default budget when the model does not give one.
    pub default_read_bytes: usize,
    /// Maximum bytes `write` accepts in one call.
    pub max_write_bytes: usize,
    /// Largest file `patch` will edit in memory.
    pub max_patch_bytes: usize,
    /// Hard cap for `list`.
    pub max_list_entries: usize,
    /// `list`'s default entry cap.
    pub default_list_entries: usize,
    /// Output cap for `git_status`/`git_diff`.
    pub max_git_output_bytes: usize,
    /// How long a git invocation may run.
    pub git_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_read_bytes: 1 << 20,      // 1 MiB
            default_read_bytes: 64 << 10, // 64 KiB
            max_write_bytes: 8 << 20,     // 8 MiB
            max_patch_bytes: 4 << 20,     // 4 MiB
            max_list_entries: 5000,
            default_list_entries: 500,
            max_git_output_bytes: 1 << 20, // 1 MiB
            git_timeout: Duration::from_secs(10),
        }
    }
}

/// The built-in coding tools, bound to one canonical workspace root.
pub struct CodingTools {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
}

impl CodingTools {
    /// Canonicalizes `root` (a symlinked root is fine) and validates it is a directory.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, WorkspaceError> {
        Ok(Self {
            root: Arc::new(WorkspaceRoot::new(root)?),
            limits: Limits::default(),
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn root(&self) -> &std::path::Path {
        self.root.root()
    }
}

impl ToolProvider for CodingTools {
    type Session = CodingSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(CodingSession {
            root: Arc::clone(&self.root),
            limits: self.limits,
            specs: specs(),
        })
    }
}

/// One session's view of the built-in tools. Holds only the root, the limits, and the
/// specs; there is no per-session state to close.
pub struct CodingSession {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
    specs: Vec<ToolSpec>,
}

fn specs() -> Vec<ToolSpec> {
    vec![read::spec(), write::spec(), patch::spec(), list::spec()]
}

impl ToolSession for CodingSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &agent_core::ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        match call.name.as_str() {
            "read" => read::run(&self.root, &self.limits, &call.arguments).await,
            "write" => write::run(&self.root, &self.limits, &call.arguments).await,
            "patch" => patch::run(&self.root, &self.limits, &call.arguments).await,
            "list" => list::run(&self.root, &self.limits, &call.arguments).await,
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the built-in coding tools"),
            )),
        }
    }
}
