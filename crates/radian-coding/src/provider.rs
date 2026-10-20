use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};

use crate::read_roots::ReadRoots;
use crate::workspace::{WorkspaceError, WorkspaceRoot};
use crate::{fail, foreground, git, glob, grep, jobs, list, patch, read, shell, write};

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
    /// Hard cap on `grep`'s total output bytes (match and context lines plus the
    /// truncation notes). Past it the tool first drops context lines to keep every match,
    /// and only then cuts whole match lines, ending with one note naming the byte limit,
    /// what was dropped, and how to narrow the search. Half of `default_read_bytes`:
    /// the data shows large results come mainly from context lines, and 32 KiB still
    /// holds about two hundred short match lines while that context bloat is cut.
    pub max_grep_output_bytes: usize,
    /// Output cap for `git_diff`.
    pub max_git_output_bytes: usize,
    /// How long a git invocation may run.
    pub git_timeout: Duration,
    /// `shell`'s output budget **per stream** when the model does not give one. Smaller than
    /// `default_read_bytes`: a shell result competes with the rest of the turn, and a truncated
    /// foreground result stays retrievable through `job` (ADR 0037 decision 3a).
    pub default_shell_output_bytes: usize,
    /// Hard cap on a model-supplied `shell` `max_output_bytes`, per stream.
    pub max_shell_output_bytes: usize,
    /// `shell`'s timeout when the model does not give one.
    pub default_shell_timeout: Duration,
    /// Hard cap on a model-supplied `timeout_secs`.
    pub max_shell_timeout: Duration,
    /// ADR 0037: a **foreground** `shell` call still running after this long is
    /// adopted by the job registry in place (same process group, never restarted), and the
    /// call returns at once with the job id. `None` disables adoption (only the foreground
    /// cap applies, as before). `Some(ZERO)` is treated as off. A host sets it from
    /// `[shell] auto_background_secs`.
    pub auto_background_after: Option<Duration>,
    /// The bytes of a **foreground** command's output retained for `shell_output` after its
    /// model-facing view was truncated. The head+tail in the result is bounded by
    /// `default_shell_output_bytes`/`max_shell_output_bytes`; this is the larger private file
    /// the middle can be recovered from. A result that hits this cap says so (the bytes past
    /// it are not recoverable).
    pub max_foreground_retained_bytes: usize,
    /// The bytes every retained foreground output together may hold per session. Keeping a new
    /// one past this evicts the oldest, whose handle then reads as unknown.
    pub max_foreground_session_bytes: usize,
    /// The number of retained foreground outputs per session, evicting the oldest past it.
    pub max_foreground_items: usize,
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
            max_grep_context: 30,
            max_grep_output_bytes: 32 << 10, // 32 KiB, half of `read`'s default
            max_git_output_bytes: 1 << 20,   // 1 MiB
            git_timeout: Duration::from_secs(10),
            // Per stream. 24 KiB sits inside the brief's 16-32 KiB band and below what other
            // agents show inline (30k chars, 50 KB, ~10k tokens ≈ 40 KiB): it halves the
            // previous 64 KiB per stream, still fits a typical failure tail, is divisible by 8
            // for the 1/8 failure head, and matches the grep cap (24-32 KiB) from the same
            // backlog item so the two truncated tools budget alike. A truncated run stays
            // recoverable through `job` (ADR 0037 decision 3a), or repeatable with a larger
            // `max_output_bytes`.
            default_shell_output_bytes: 24 << 10,
            // The model may ask for more, up to `read`'s own hard cap.
            max_shell_output_bytes: 1 << 20,
            default_shell_timeout: Duration::from_secs(60),
            max_shell_timeout: Duration::from_secs(600),
            auto_background_after: Some(Duration::from_secs(120)),
            // The model-facing view keeps a bounded head+tail (24 KiB per stream by default,
            // split 1/4 : 3/4 on success and 1/8 : 7/8 on failure); the retained file is larger
            // the retained file is larger so the discarded middle can be recovered, but still
            // bounded per item and per session. 8 MiB per item covers a large build log's
            // middle; 32 MiB per session keeps four such items, and eviction drops the oldest.
            max_foreground_retained_bytes: 8 << 20,
            max_foreground_session_bytes: 32 << 20,
            max_foreground_items: 32,
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
    /// command without one. The read-only registry and toolchain roots are resolved from the
    /// environment (ADR 0039).
    pub fn new(root: impl AsRef<std::path::Path>) -> Result<Self, WorkspaceError> {
        Self::with_read_roots(root, ReadRoots::from_env())
    }

    /// [`CodingTools::new`] with an explicit set of read-only roots instead of the environment
    /// (`ReadRoots::none()` keeps the file tools workspace-only). For tests and embedders.
    pub fn with_read_roots(
        root: impl AsRef<std::path::Path>,
        read_roots: ReadRoots,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self::from_root(
            WorkspaceRoot::with_read_roots(root, read_roots)?,
            SandboxSlot::empty(),
            None,
        ))
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
        let root = WorkspaceRoot::with_read_roots(root, ReadRoots::from_env())?;
        if let Some(current) = sandbox.get()
            && current.policy().workspace != root.root()
        {
            return Err(CodingToolsError::SandboxWorkspace {
                tools: root.root().to_path_buf(),
                sandbox: current.policy().workspace.clone(),
            });
        }
        Ok(Self::from_root(root, sandbox, None))
    }

    /// Assembles the provider from an already-built root (read-only roots included), the
    /// sandbox slot and an optional job registry.
    fn from_root(root: WorkspaceRoot, sandbox: SandboxSlot, jobs: Option<Arc<jobs::Jobs>>) -> Self {
        Self {
            root: Arc::new(root),
            limits: Limits::default(),
            sandbox,
            jobs,
        }
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
        let (jobs, foreground) = match &self.jobs {
            Some(jobs) => {
                let jobs = Arc::clone(jobs);
                // Retained foreground output lives in the caller-supplied, session-private jobs
                // directory (ADR 0039 decision 1) and is registered in the same job registry
                // (ADR 0037 decision 3a: one id space). The registry is *per session*.
                let foreground = Arc::new(foreground::ForegroundOutputs::new(
                    jobs.dir().to_path_buf(),
                    &self.limits,
                    &jobs,
                ));
                (jobs, Some(foreground))
            }
            // A provider built without a caller-supplied private directory (a test, an embedded
            // tool set) falls back to a per-workspace jobs directory for `job_output` — but a
            // workspace directory is readable by the model's `read`, so foreground retention is
            // **off**: there is nowhere safe to put it. No handle is created and `shell_output`
            // is not advertised; the ordinary truncation notice still says bytes were discarded.
            None => (
                Arc::new(jobs::Jobs::new(
                    self.root.root().join(".radian").join("jobs"),
                )),
                None,
            ),
        };
        // The tool set is fixed here, once per session (ADR 0036): the
        // shell description therefore reflects the tier the session **started** in. A later
        // `/access` swap changes what runs, not the advertised text (the `/access` notice says
        // so). `git_writable` is the sandbox's effective answer, not the policy's request.
        let sandbox = self.sandbox.get();
        let shell = sandbox
            .as_ref()
            .map(|sandbox| (sandbox.mode(), sandbox.git_writable_except_exec()));
        Ok(CodingSession {
            root: Arc::clone(&self.root),
            limits: self.limits,
            sandbox: self.sandbox.clone(),
            specs: specs(shell, foreground.is_some()),
            jobs,
            foreground,
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
    /// Retained foreground `shell` output, read by `shell_output`. `None` when the provider has
    /// no caller-supplied private directory (see [`CodingTools::open_session`]), in which case
    /// neither a handle nor the tool exists.
    foreground: Option<Arc<foreground::ForegroundOutputs>>,
}

impl CodingSession {
    /// The session's background-job registry. Slice 3 subscribes to settled jobs here; the
    /// host's session shutdown calls [`CodingSession::shutdown_jobs`].
    pub fn jobs(&self) -> &Arc<jobs::Jobs> {
        &self.jobs
    }

    /// Session end (ADR 0037 decision 6): kill the whole session of every running job. No file
    /// I/O, so the panic path may call it too.
    pub fn shutdown_jobs(&self) {
        self.jobs.kill_all();
    }

    /// Session end: drop the retained foreground output and remove its files. Synchronous file
    /// removal only; the registry state also drops with the session.
    pub fn shutdown_outputs(&self) {
        if let Some(foreground) = &self.foreground {
            foreground.shutdown();
        }
    }
}

fn specs(shell: Option<(SandboxMode, bool)>, foreground: bool) -> Vec<ToolSpec> {
    let mut specs = vec![
        read::spec(),
        write::spec(),
        patch::spec(),
        list::spec(),
        glob::spec(),
        grep::spec(),
        git::diff_spec(),
        git::status_alias_spec(),
    ];
    if let Some((mode, git_writable)) = shell {
        specs.push(shell::spec(mode, git_writable));
        // The job tool exists only with `shell`: without a sandbox there are no jobs. The
        // four old names stay registered as one-version aliases (ADR 0043 decision 3): the
        // host strips them from every model request, so the model never sees them.
        specs.push(jobs::job_spec());
        specs.push(jobs::output_alias_spec());
        specs.push(jobs::list_alias_spec());
        specs.push(jobs::kill_alias_spec());
        // Only a `shell` call produces retained foreground output, and only when there is a
        // caller-supplied private directory to keep it in.
        if foreground {
            specs.push(foreground::alias_spec());
        }
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
                        self.foreground.as_deref(),
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
            "job" => jobs::run_job(&self.jobs, &self.limits, &call.arguments, &cx).await,
            "job_output" => {
                jobs::run_output_alias(&self.jobs, &self.limits, &call.arguments, &cx).await
            }
            "job_list" => jobs::run_list_alias(&self.jobs),
            "job_kill" => jobs::run_kill_alias(&self.jobs, &call.arguments),
            "shell_output" => foreground::run_alias(&self.jobs, &self.limits, &call.arguments),
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the built-in coding tools"),
            )),
        }
    }
}
