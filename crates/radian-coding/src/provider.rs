use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use radian_core::{
    ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use radian_sandbox::{Sandbox, SandboxMode, SandboxSlot};

use crate::deny::DenySet;
use crate::read_roots::ReadRoots;
use crate::workspace::{WorkspaceError, WorkspaceRoot};
use crate::{fail, foreground, glob, grep, jobs, patch, read, shell, write};

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
    /// Hard cap for a directory `read` (the old `list`).
    pub max_list_entries: usize,
    /// A directory `read`'s default entry cap.
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
    /// The bytes of a **foreground** command's output retained after its
    /// model-facing view was truncated. The head+tail in the result is bounded by
    /// `default_shell_output_bytes`/`max_shell_output_bytes`; this is the larger private file
    /// the middle can be recovered from. A result that hits this cap says so (the bytes past
    /// it are not recoverable).
    pub max_foreground_retained_bytes: usize,
    /// The bytes every retained foreground output together may hold per session. Keeping a new
    /// one past this evicts the oldest, whose id then reads as unknown.
    pub max_foreground_session_bytes: usize,
    /// The number of retained foreground outputs per session, evicting the oldest past it.
    pub max_foreground_items: usize,
}

impl Limits {
    /// `shell`'s timeout when the model does not give one (ADR 0037 decision 2): the same
    /// bound the one-step-foreground hint in `job` results measures against (ADR 0037
    /// decision 4).
    pub const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(300);
    /// Hard cap on a model-supplied `timeout_secs` (ADR 0037 decision 2).
    pub const MAX_SHELL_TIMEOUT: Duration = Duration::from_secs(600);
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
            default_shell_timeout: Self::DEFAULT_SHELL_TIMEOUT,
            max_shell_timeout: Self::MAX_SHELL_TIMEOUT,
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

/// Which of the built-in tools a session's request carries (ADR 0043 §4). A tool's
/// description names another tool only when `ToolPresence` says that tool is mounted, so a
/// restricted set (the hidden `--tools` list) never points the model at a tool it cannot call.
/// [`ToolProvider::open_session`] uses [`ToolPresence::ALL`], which keeps today's text byte
/// for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPresence {
    pub read: bool,
    pub write: bool,
    pub patch: bool,
    pub glob: bool,
    pub grep: bool,
    pub shell: bool,
    pub job: bool,
}

impl ToolPresence {
    /// Every tool: the full set's descriptions and specs, unchanged.
    pub const ALL: Self = Self {
        read: true,
        write: true,
        patch: true,
        glob: true,
        grep: true,
        shell: true,
        job: true,
    };

    /// The tools `names` holds. Each name is matched exactly.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let names: Vec<&str> = names.into_iter().collect();
        let has = |name: &str| names.contains(&name);
        Self {
            read: has("read"),
            write: has("write"),
            patch: has("patch"),
            glob: has("glob"),
            grep: has("grep"),
            shell: has("shell"),
            job: has("job"),
        }
    }

    /// The tools an allow-list names. `None` mounts every built-in (and keeps today's text).
    pub fn from_allow(allow: Option<&[String]>) -> Self {
        match allow {
            None => Self::ALL,
            Some(names) => Self::from_names(names.iter().map(String::as_str)),
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
    /// Whether the current model takes images (ADR 0018). The host supplies a closure over the
    /// live route, so a `/model` switch changes the `read` tool's answer at the next call; the
    /// default counts as taking images (the catalog's rule for an unknown model).
    image_support: crate::ImageSupport,
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
        Self::with_read_roots_and_deny(root, read_roots, DenySet::empty())
    }

    /// [`CodingTools::with_read_roots`] with an explicit `full`-tier deny set. The tier source
    /// is empty, which reads as the `workspace` tier (outside paths outside the roots refuse).
    /// For tests of the `workspace`-tier boundary.
    pub fn with_read_roots_and_deny(
        root: impl AsRef<std::path::Path>,
        read_roots: ReadRoots,
        deny: DenySet,
    ) -> Result<Self, WorkspaceError> {
        Ok(Self::from_root(
            WorkspaceRoot::with_access(root, read_roots, deny, SandboxSlot::empty())?,
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
    /// `open_session`, so a session that starts without `shell` does not gain it mid-session
    /// (switching to a *narrower* tier still denies it through the permission rules).
    ///
    /// The file tools read the same slot at every call, so a tier switch changes what they
    /// read at the next call (ADR 0039 decision 4). The deny set defaults to empty.
    pub fn with_sandbox_slot(
        root: impl AsRef<std::path::Path>,
        sandbox: SandboxSlot,
    ) -> Result<Self, CodingToolsError> {
        Self::with_sandbox_slot_and_access(root, sandbox, ReadRoots::from_env(), DenySet::empty())
    }

    /// [`CodingTools::with_sandbox_slot`] with explicit read-only roots (the host passes the
    /// skill roots from its skill discovery) and the `full`-tier deny set (the host passes
    /// its deny-read list). The slot is both the `shell` source and the file tools' tier
    /// source: `FullAccess` reads any absolute or `~` path except the deny set, any other
    /// mode (or an empty slot) keeps outside paths to the roots.
    pub fn with_sandbox_slot_and_access(
        root: impl AsRef<std::path::Path>,
        sandbox: SandboxSlot,
        read_roots: ReadRoots,
        deny: DenySet,
    ) -> Result<Self, CodingToolsError> {
        let root = WorkspaceRoot::with_access(root, read_roots, deny, sandbox.clone())?;
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
            image_support: crate::image::images_allowed(),
            sandbox,
            jobs,
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Supply the current model's image capability (ADR 0018). The host's closure reads the
    /// live route and the catalog; the default takes images.
    pub fn with_image_support(mut self, image_support: crate::ImageSupport) -> Self {
        self.image_support = image_support;
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

impl CodingTools {
    /// Open one session whose specs and descriptions are restricted to `presence` (ADR 0043
    /// §4). The hidden `--tools` list is the caller: a tool outside the list is not built at
    /// all, and a description never names it. [`ToolProvider::open_session`] passes
    /// [`ToolPresence::ALL`], so an ordinary session is unchanged.
    pub async fn open_session_with(
        &self,
        presence: ToolPresence,
    ) -> Result<CodingSession, ToolError> {
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
            // tool set) falls back to a per-workspace jobs directory — but a
            // workspace directory is readable by the model's `read`, so foreground retention is
            // **off**: there is nowhere safe to put it. No id is registered for an
            // untruncated result's middle; the ordinary truncation notice still says bytes
            // were discarded.
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
            image_support: Arc::clone(&self.image_support),
            sandbox: self.sandbox.clone(),
            specs: specs(shell, presence),
            jobs,
            foreground,
        })
    }
}

impl ToolProvider for CodingTools {
    type Session = CodingSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        self.open_session_with(ToolPresence::ALL).await
    }
}

/// One session's view of the built-in tools. Holds the root, the limits, the sandbox, the
/// specs and the background-job registry (ADR 0037).
pub struct CodingSession {
    root: Arc<WorkspaceRoot>,
    limits: Limits,
    /// The current model's image capability (ADR 0018), asked at each `read` call.
    image_support: crate::ImageSupport,
    sandbox: SandboxSlot,
    specs: Vec<ToolSpec>,
    jobs: Arc<jobs::Jobs>,
    /// Retained foreground `shell` output, read back through the `job` tool by id
    /// (ADR 0037 decision 3a: one id space). `None` when the provider has
    /// no caller-supplied private directory (see [`CodingTools::open_session`]), in which case
    /// a truncated result's middle is not recoverable.
    foreground: Option<Arc<foreground::ForegroundOutputs>>,
}

impl CodingSession {
    /// The session's background-job registry. Slice 3 subscribes to settled jobs here; the
    /// host's session shutdown calls [`CodingSession::shutdown_jobs`].
    pub fn jobs(&self) -> &Arc<jobs::Jobs> {
        &self.jobs
    }

    /// Session end (ADR 0037 decision 6): kill the whole session of every running job and its
    /// descendant tree. Signals and process reads only, so the panic path may call it too.
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

fn specs(shell: Option<(SandboxMode, bool)>, presence: ToolPresence) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    if presence.read {
        specs.push(read::spec(presence));
    }
    if presence.write {
        specs.push(write::spec());
    }
    if presence.patch {
        specs.push(patch::spec());
    }
    if presence.glob {
        specs.push(glob::spec());
    }
    if presence.grep {
        specs.push(grep::spec());
    }
    if let Some((mode, git_writable)) = shell
        && presence.shell
    {
        specs.push(shell::spec(mode, git_writable, presence));
        // The job tool exists only with `shell`: without a sandbox there are no jobs.
        if presence.job {
            specs.push(jobs::job_spec());
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
        call: &radian_core::ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        match call.name.as_str() {
            "read" => {
                read::run(
                    &self.root,
                    &self.limits,
                    &self.image_support,
                    &call.arguments,
                )
                .await
            }
            "write" => write::run(&self.root, &self.limits, &call.arguments).await,
            "patch" => patch::run(&self.root, &self.limits, &call.arguments).await,
            "glob" => glob::run(&self.root, &self.limits, &call.arguments).await,
            "grep" => grep::run(&self.root, &self.limits, &call.arguments).await,
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
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the built-in coding tools"),
            )),
        }
    }
}
