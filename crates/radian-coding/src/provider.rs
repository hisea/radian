use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};

use crate::workspace::{WorkspaceError, WorkspaceRoot};
use crate::{fail, git, glob, grep, jobs, list, patch, read, shell, write};

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
    /// Hard cap for one `glob` result set.
    pub max_glob_results: usize,
    /// `glob`'s default result cap when the model does not give one.
    pub default_glob_results: usize,
    /// Hard cap for `grep`'s total matches.
    pub max_grep_matches: usize,
    /// `grep`'s default total match cap when the model does not give one.
    pub default_grep_matches: usize,
    /// `grep`'s per-file match cap. Not model-settable: one file must not fill the result.
    pub default_grep_matches_per_file: usize,
    /// Hard cap on a model-supplied `grep` `context` line count.
    pub max_grep_context: usize,
    /// Output cap for `git_status`/`git_diff`.
    pub max_git_output_bytes: usize,
    /// How long a git invocation may run.
    pub git_timeout: Duration,
    /// `shell`'s output budget **per stream** when the model does not give one. Aligned with
    /// `default_read_bytes`: one tool result should not be able to dominate the context.
    pub default_shell_output_bytes: usize,
    /// Hard cap on a model-supplied `shell` `max_output_bytes`, per stream.
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
            max_glob_results: 5000,
            default_glob_results: 500,
            max_grep_matches: 5000,
            default_grep_matches: 200,
            default_grep_matches_per_file: 50,
            max_grep_context: 10,
            max_git_output_bytes: 1 << 20, // 1 MiB
            git_timeout: Duration::from_secs(10),
            // Per stream, and aligned with `read`'s default budget: 64 KiB is enough for the
            // interesting part of a build or test log, and a truncated run can be repeated
            // with a narrower command or a larger `max_output_bytes`.
            default_shell_output_bytes: 64 << 10,
            // The model may ask for more, up to `read`'s own hard cap.
            max_shell_output_bytes: 1 << 20,
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
    /// The sandbox `shell`/jobs read at the start of every call (ADR 0038 slice 2). The host
    /// keeps a clone and swaps it when `/access` changes the tier; a call already running
    /// holds the `Arc` it cloned and is unaffected.
    sandbox: SandboxSlot,
    /// The background-job registry (ADR 0037). The host supplies one so it can subscribe to
    /// job lifecycle notices; when it does not, `open_session` creates one.
    jobs: Option<Arc<jobs::Jobs>>,
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
            sandbox: SandboxSlot::empty(),
            jobs: None,
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
        Self::with_sandbox_slot(root, SandboxSlot::with(sandbox))
    }

    /// The same tools over a **shared** sandbox slot (ADR 0038 slice 2): the host keeps a
    /// clone and swaps it on `/access`, so `shell`/jobs read the current tier. `shell` is
    /// advertised whenever the slot currently holds a sandbox; the tool set is fixed at
    /// `open_session`, so a session that starts in `safe` does not gain `shell` mid-session
    /// (switching to a *narrower* tier still denies it through the permission rules).
    pub fn with_sandbox_slot(
        root: impl AsRef<std::path::Path>,
        sandbox: SandboxSlot,
    ) -> Result<Self, CodingToolsError> {
        let root = WorkspaceRoot::new(root)?;
        if let Some(current) = sandbox.get()
            && current.policy().workspace != root.root()
        {
            return Err(CodingToolsError::SandboxWorkspace {
                tools: root.root().to_path_buf(),
                sandbox: current.policy().workspace.clone(),
            });
        }
        Ok(Self {
            root: Arc::new(root),
            limits: Limits::default(),
            sandbox,
            jobs: None,
        })
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Supplies the background-job registry (ADR 0037 slice 3): the host creates it so it can
    /// subscribe to [`jobs::Jobs::subscribe`] and write the durable start/settle records.
    pub fn with_jobs(mut self, jobs: Arc<jobs::Jobs>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    pub fn root(&self) -> &std::path::Path {
        self.root.root()
    }

    /// The sandbox `shell` runs under, if the tool is present.
    pub fn sandbox_mode(&self) -> Option<SandboxMode> {
        self.sandbox.mode()
    }
}

impl ToolProvider for CodingTools {
    type Session = CodingSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        let jobs = match &self.jobs {
            Some(jobs) => Arc::clone(jobs),
            None => Arc::new(jobs::Jobs::new(self.root.root())),
        };
        Ok(CodingSession {
            root: Arc::clone(&self.root),
            limits: self.limits,
            sandbox: self.sandbox.clone(),
            specs: specs(self.sandbox.mode()),
            jobs,
        })
    }
}

/// One session's view of the built-in tools. Holds the root, the limits, the sandbox, the
/// specs and the background-job registry (ADR 0037).
pub struct CodingSession {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
    sandbox: SandboxSlot,
    specs: Vec<ToolSpec>,
    jobs: Arc<jobs::Jobs>,
}

impl CodingSession {
    /// The session's background-job registry. Slice 3 subscribes to settled jobs here; the
    /// host's session shutdown calls [`CodingSession::shutdown_jobs`].
    pub fn jobs(&self) -> &Arc<jobs::Jobs> {
        &self.jobs
    }

    /// Session end (ADR 0037 decision 6): kill every running job's process group. No file I/O,
    /// so the panic path may call it too.
    pub fn shutdown_jobs(&self) {
        self.jobs.kill_all();
    }
}

fn specs(shell_mode: Option<SandboxMode>) -> Vec<ToolSpec> {
    let mut specs = vec![
        read::spec(),
        write::spec(),
        patch::spec(),
        list::spec(),
        glob::spec(),
        grep::spec(),
        git::status_spec(),
        git::diff_spec(),
    ];
    if let Some(mode) = shell_mode {
        specs.push(shell::spec(mode));
        // The job tools exist only with `shell`: without a sandbox there are no jobs.
        specs.push(jobs::output_spec());
        specs.push(jobs::list_spec());
        specs.push(jobs::kill_spec());
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
            "glob" => glob::run(&self.root, &self.limits, &call.arguments).await,
            "grep" => grep::run(&self.root, &self.limits, &call.arguments).await,
            "git_status" => git::status(&self.root, &self.limits, &call.arguments).await,
            "git_diff" => git::diff(&self.root, &self.limits, &call.arguments).await,
            "shell" => match self.sandbox.get() {
                Some(sandbox) => {
                    shell::run(
                        &self.root,
                        &sandbox,
                        &self.limits,
                        &self.jobs,
                        &call.arguments,
                        &cx,
                    )
                    .await
                }
                // Unreachable through the advertised specs; kept as a refusal rather than a
                // fallback so a hand-built call can never run unsandboxed.
                None => Err(fail(
                    "unknown_tool",
                    "`shell` needs a sandbox and this session has none",
                )),
            },
            "job_output" => jobs::run_output(&self.jobs, &call.arguments, &cx).await,
            "job_list" => jobs::run_list(&self.jobs),
            "job_kill" => jobs::run_kill(&self.jobs, &call.arguments),
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the built-in coding tools"),
            )),
        }
    }
}
