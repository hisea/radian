use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_sandbox::{Sandbox, SandboxMode};

use crate::workspace::{WorkspaceError, WorkspaceRoot};
use crate::{fail, git, list, patch, read, shell, write};

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
    /// Combined stdout+stderr cap for one `shell` call.
    pub max_shell_output_bytes: usize,
    /// `shell`'s timeout when the model does not give one.
    pub default_shell_timeout: Duration,
    /// Hard cap on a model-supplied `timeout_secs`.
    pub max_shell_timeout: Duration,
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
            // 256 KiB combined is four times the default `read` budget: enough for a build
            // or test log tail, small enough that it cannot dominate the model context. A
            // truncated run can be repeated with a narrower command.
            max_shell_output_bytes: 256 << 10,
            default_shell_timeout: Duration::from_secs(60),
            max_shell_timeout: Duration::from_secs(600),
        }
    }
}

/// Why a [`CodingTools`] could not be built.
#[derive(Debug, thiserror::Error)]
pub enum CodingToolsError {
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error(
        "the sandbox workspace `{sandbox}` does not match the tools' workspace `{tools}`; \
         they must be the same canonical directory"
    )]
    SandboxWorkspace { tools: PathBuf, sandbox: PathBuf },
}

/// The built-in coding tools, bound to one canonical workspace root.
///
/// `shell` is present **only** when the tools were built with a sandbox
/// ([`CodingTools::with_sandbox`]). A provider without one does not advertise the tool at
/// all, so a model never asks for a naked run, and there is no fallback path that would run
/// it unsandboxed.
pub struct CodingTools {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
    sandbox: Option<Arc<Sandbox>>,
}

impl CodingTools {
    /// Canonicalizes `root` (a symlinked root is fine) and validates it is a directory.
    ///
    /// The result has **no** `shell` tool: there is no sandbox, and this slice never runs a
    /// command without one.
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, WorkspaceError> {
        Ok(Self {
            root: Arc::new(WorkspaceRoot::new(root)?),
            limits: Limits::default(),
            sandbox: None,
        })
    }

    /// The same tools, plus `shell`, confined by `sandbox`.
    ///
    /// The sandbox must already exist, which means an enforced mode already passed its probe
    /// ([`Sandbox::new`] refuses otherwise, ADR 0012). `FullAccess` is only reachable because
    /// the caller named it when building the sandbox.
    pub fn with_sandbox(
        root: impl AsRef<std::path::Path>,
        sandbox: Sandbox,
    ) -> Result<Self, CodingToolsError> {
        let root = WorkspaceRoot::new(root)?;
        if sandbox.policy().workspace != root.root() {
            return Err(CodingToolsError::SandboxWorkspace {
                tools: root.root().to_path_buf(),
                sandbox: sandbox.policy().workspace.clone(),
            });
        }
        Ok(Self {
            root: Arc::new(root),
            limits: Limits::default(),
            sandbox: Some(Arc::new(sandbox)),
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    pub fn root(&self) -> &std::path::Path {
        self.root.root()
    }

    /// The sandbox `shell` runs under, if the tool is present.
    pub fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.sandbox.as_deref().map(Sandbox::mode)
    }
}

impl ToolProvider for CodingTools {
    type Session = CodingSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(CodingSession {
            root: Arc::clone(&self.root),
            limits: self.limits,
            sandbox: self.sandbox.clone(),
            specs: specs(self.sandbox.as_deref().map(Sandbox::mode)),
        })
    }
}

/// One session's view of the built-in tools. Holds only the root, the limits, the sandbox
/// and the specs; there is no per-session state to close.
pub struct CodingSession {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
    sandbox: Option<Arc<Sandbox>>,
    specs: Vec<ToolSpec>,
}

fn specs(shell_mode: Option<SandboxMode>) -> Vec<ToolSpec> {
    let mut specs = vec![
        read::spec(),
        write::spec(),
        patch::spec(),
        list::spec(),
        git::status_spec(),
        git::diff_spec(),
    ];
    if let Some(mode) = shell_mode {
        specs.push(shell::spec(mode));
    }
    specs
}

impl ToolSession for CodingSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &agent_core::ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        match call.name.as_str() {
            "read" => read::run(&self.root, &self.limits, &call.arguments).await,
            "write" => write::run(&self.root, &self.limits, &call.arguments).await,
            "patch" => patch::run(&self.root, &self.limits, &call.arguments).await,
            "list" => list::run(&self.root, &self.limits, &call.arguments).await,
            "git_status" => git::status(&self.root, &self.limits, &call.arguments).await,
            "git_diff" => git::diff(&self.root, &self.limits, &call.arguments).await,
            "shell" => match &self.sandbox {
                Some(sandbox) => {
                    shell::run(&self.root, sandbox, &self.limits, &call.arguments, &cx).await
                }
                // Unreachable through the advertised specs; kept as a refusal rather than a
                // fallback so a hand-built call can never run unsandboxed.
                None => Err(fail(
                    "unknown_tool",
                    "`shell` needs a sandbox and this session has none",
                )),
            },
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the built-in coding tools"),
            )),
        }
    }
}
