//! OS sandbox backends for the coding tools: macOS `sandbox-exec` (SBPL) and Linux
//! `bubblewrap`.
//!
//! The rule this crate exists to enforce (ADR 0012, decision 2): **a requested
//! enforcement mode that has no working backend fails loudly and never falls back to an
//! unsandboxed run.** [`Sandbox::new`] and [`Sandbox::command`] have no "degrade" path;
//! `FullAccess` is only ever reached because the caller asked for it by name.
//!
//! Two consequences are stated here rather than hidden:
//!
//! - The macOS backend leaves `mach-lookup` open, so **IPC is not contained**. "Sensitive
//!   directories are not readable" is **not** a claim of full credential isolation; no
//!   user-facing text may present it as one (ADR 0012 P4/P6).
//! - Environment filtering is part of the spawn path in this crate, not a sandbox feature.
//!
//! A backend is only trusted after [`SandboxBackend::probe`] runs a real probe and proves a
//! deny actually takes effect. The binary existing is not enough: SBPL operation names are a
//! private interface that has silently drifted between macOS releases.

#[cfg(target_os = "linux")]
mod bwrap;
mod deny;
mod error;
mod git_exec;
mod inherited;
mod policy;
#[cfg(target_os = "macos")]
mod seatbelt;
#[cfg(target_os = "linux")]
mod seccomp;
mod slot;

#[cfg(feature = "testing")]
pub mod harness;

use std::ffi::{OsStr, OsString};
use std::ops::{Deref, DerefMut};
use std::process::Command;

pub use deny::{DenialCategory, MARKER_PREFIX, classify_denial};
pub use error::{ProbeReport, SandboxError};
pub use git_exec::{
    GIT_EXEC_DIRS, GIT_EXEC_FILES, GIT_MODULE_DIRS, GIT_MODULE_FILES, GIT_WORKTREE_FILES,
    is_git_exec_entry,
};
pub use policy::{
    DEFAULT_ALLOW_ENV, DEFAULT_DENY_READ, GitAccess, SandboxMode, SandboxPolicy,
    discover_git_metadata,
};
pub use slot::SandboxSlot;

/// A concrete sandbox mechanism. `command` returns a launcher `Command` that runs `program`
/// with `args` under the policy; the caller may add cwd/stdio but must not widen the
/// environment (the returned command already cleared it).
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Runs a probe that proves the mechanism actually denies something. Returning `Ok` is
    /// the only way a backend may be used for an enforced mode.
    fn probe(&self) -> Result<ProbeReport, SandboxError>;

    /// Builds the launcher command. `program`/`args` are the child's argv; the backend
    /// prepends its own launcher and options. The returned [`SandboxCommand`] owns any file
    /// descriptor the launcher needs (on Linux, the seccomp program); dropping it closes the
    /// fd, so building commands in a loop cannot leak.
    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError>;

    /// Whether this backend **enforces** [`GitAccess::WritableExceptExec`] for `policy`
    /// (ADR 0038 decision 4): the discovered git metadata is writable except the entries git
    /// can execute. The default is `false`: a backend that does not implement the carve-out
    /// keeps the whole metadata read-only whatever the policy asks for, and the safe default
    /// means a new backend can never claim a writability it does not enforce (Linux is still
    /// pending). The shell tool's description and the startup line read this — via
    /// [`Sandbox::git_writable_except_exec`] — not `policy.git_access`, so they cannot claim
    /// that native `git commit` works when the backend would deny it.
    fn git_writable_except_exec(&self, _policy: &SandboxPolicy) -> bool {
        false
    }

    /// Whether this backend **denies** outbound Unix socket connections when the network is
    /// on (ADR 0016). The default is `false`: a backend that does not filter
    /// Unix-socket connects must never claim it does, so the `<runtime>` prompt layer —
    /// via [`Sandbox::restricts_unix_sockets`] — cannot tell the model that outbound Unix
    /// sockets are denied when they are reachable. macOS `seatbelt` returns the policy's
    /// network flag (the deny rule is only emitted with `(allow network*)`); Linux `bwrap`
    /// keeps the default `false` (pathname sockets stay reachable through the shared
    /// `--ro-bind / /`, abstract sockets through the shared network namespace in
    /// `workspace`), as do the `inherited` and transparent backends.
    fn restricts_unix_sockets(&self, _policy: &SandboxPolicy) -> bool {
        false
    }
}

/// A sandboxed launcher command: a [`std::process::Command`] plus ownership of any file
/// descriptor the launcher needs (on Linux, the read end of the pipe holding the compiled
/// seccomp program, inherited by `bwrap` under `--seccomp`).
///
/// It derefs to the inner command, so `stdin`/`spawn`/`output`/`pre_exec` (the shell tool
/// registers its own `pre_exec` for `setsid` on the same command) all work unchanged; what
/// it adds is lifetime: dropping the command closes the fd, whether the command was
/// spawned or not. Each command owns a freshly written pipe, so concurrent builds never
/// share an offset. There is deliberately no `Clone` and no `into_inner`: the fd must not
/// outlive the command it was built for.
#[derive(Debug)]
pub struct SandboxCommand {
    command: Command,
    /// Linux `workspace` only: the seccomp program `bwrap` reads. `None` everywhere else
    /// (`full`, `inherited`, macOS).
    #[cfg(target_os = "linux")]
    _seccomp_filter: Option<std::os::fd::OwnedFd>,
}

impl SandboxCommand {
    /// Wraps a plain command (backends with no launcher fd: `full`, `inherited`, macOS).
    pub fn new(command: Command) -> Self {
        Self {
            command,
            #[cfg(target_os = "linux")]
            _seccomp_filter: None,
        }
    }

    /// A command whose launcher needs `filter` inherited (Linux `workspace`'s `--seccomp`).
    #[cfg(target_os = "linux")]
    pub(crate) fn with_seccomp_filter(command: Command, filter: std::os::fd::OwnedFd) -> Self {
        Self {
            command,
            _seccomp_filter: Some(filter),
        }
    }
}

impl Deref for SandboxCommand {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for SandboxCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

/// The backend used for `FullAccess`: it wraps nothing. It is never chosen for an enforced
/// mode.
pub struct NoSandbox;

static NO_SANDBOX: NoSandbox = NoSandbox;

impl SandboxBackend for NoSandbox {
    fn name(&self) -> &'static str {
        "none"
    }

    fn probe(&self) -> Result<ProbeReport, SandboxError> {
        Ok(ProbeReport {
            backend: self.name(),
            detail: "no sandbox requested".into(),
        })
    }

    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<SandboxCommand, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(&policy.workspace);
        Ok(SandboxCommand::new(command))
    }
}

/// The backend for the current OS, if one is compiled in.
pub fn platform_backend() -> Result<&'static dyn SandboxBackend, SandboxError> {
    #[cfg(target_os = "macos")]
    {
        return Ok(&seatbelt::SEATBELT);
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(&bwrap::BWRAP);
    }
    #[allow(unreachable_code)]
    Err(SandboxError::UnsupportedPlatform(std::env::consts::OS))
}

/// ADR 0016: whether the **kernel** says this process is already confined
/// by a sandbox. macOS asks `sandbox_check(getpid(), NULL, 0)`; anything else is `false`.
///
/// This is the only source of that fact. No environment variable, config key or flag may stand
/// in for it: a script outside any sandbox could set a variable, and radian would then run with
/// no confinement at all. A missing symbol or an error (a negative result) also reads as
/// `false`, so a future macOS that drops the private API falls back to the ordinary
/// probe-and-refuse path — fail closed, never fail open.
///
/// Linux is deliberately unanswered (revision 2 decision 7): `bwrap` nesting and a kernel
/// signal for "already confined" are unverified there, so Linux keeps today's behaviour.
pub fn confined_by_outer_sandbox() -> bool {
    #[cfg(target_os = "macos")]
    {
        return macos::sandbox_check_self();
    }
    #[allow(unreachable_code)]
    false
}

/// The private macOS API behind [`confined_by_outer_sandbox`].
#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::{c_char, c_int};

    // `libsystem_sandbox.dylib` exports `sandbox_check`; libSystem's shared cache resolves it,
    // and there is no public header for it (`sandbox.h` declares only the deprecated
    // `sandbox_init`). The signature is the measured one:
    // `int sandbox_check(pid_t pid, const char *operation, int type, ...)`.
    unsafe extern "C" {
        fn sandbox_check(pid: c_int, operation: *const c_char, kind: c_int, ...) -> c_int;
    }

    /// `sandbox_check(getpid(), NULL, 0)`: 0 outside a sandbox, 1 under `sandbox-exec` with
    /// either an `(allow default)` or a `(deny default)` profile (measured on macOS 27,
    /// 2026-10-16). The `type` argument's meaning is undocumented and untested here; 0 is what
    /// was measured and is what is used.
    pub(super) fn sandbox_check_self() -> bool {
        let pid = std::process::id() as c_int;
        // SAFETY: `sandbox_check` only inspects the named process's sandbox state. With a null
        // `operation` and `kind = 0` it takes no further arguments (the variadic tail is
        // unused), and it has no memory-safety preconditions.
        let result = unsafe { sandbox_check(pid, std::ptr::null(), 0) };
        result > 0
    }
}

/// ADR 0020: the workspace must not be inside a path the sandbox denies reading, or the
/// sandbox would hide the workspace from the commands it is supposed to let run. Refuse the
/// enforced modes before any backend probes.
fn validate_workspace(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    if let Some(denied) = crate::policy::deny_read_covers_workspace(policy) {
        return Err(SandboxError::InvalidWorkspace {
            path: policy.workspace.clone(),
            detail: format!(
                "the workspace is inside {}, which the sandbox does not let commands read \
                 (ADR 0020); move the session directory or the workspace",
                denied.display()
            ),
        });
    }
    Ok(())
}

/// A ready-to-use sandbox. Constructing it probes the backend; the probe must succeed or
/// construction fails. `Clone` is cheap (a policy and a `'static` backend): the TUI keeps one
/// clone for the user's `!`/`!!` commands while the coding tools own another (ADR 0033).
#[derive(Clone)]
pub struct Sandbox {
    backend: &'static dyn SandboxBackend,
    policy: SandboxPolicy,
    probe: ProbeReport,
}

impl Sandbox {
    /// Builds a sandbox for `mode` and `workspace`.
    ///
    /// `WorkspaceWrite` requires a working backend: if the tool is missing or the probe
    /// fails, this returns an error and the caller must refuse to run. `FullAccess` returns a
    /// transparent sandbox without probing, because nothing is being enforced.
    pub fn new(
        mode: SandboxMode,
        workspace: impl AsRef<std::path::Path>,
    ) -> Result<Self, SandboxError> {
        let policy = SandboxPolicy::new(mode, workspace)?;
        Self::from_policy(policy)
    }

    /// ADR 0016: the sandbox for a process the kernel says is **already
    /// confined** by an outer sandbox. radian adds no layer of its own — there is no policy
    /// bookkeeping to do locally (the outer sandbox owns what is readable, writable and
    /// reachable) and no probe to run (a nested `sandbox-exec` cannot start at all) — so this
    /// builds the [`inherited`](crate::inherited) backend directly.
    ///
    /// Callers reach it only through [`confined_by_outer_sandbox`]: it is never a fallback.
    pub fn inherited(workspace: impl AsRef<std::path::Path>) -> Self {
        Self {
            backend: &inherited::INHERITED,
            policy: SandboxPolicy::inherited(workspace.as_ref()),
            probe: ProbeReport {
                backend: "inherited",
                detail: inherited::PROBE_DETAIL.into(),
            },
        }
    }

    pub fn from_policy(policy: SandboxPolicy) -> Result<Self, SandboxError> {
        if !policy.mode.is_enforced() {
            return Ok(Self {
                backend: &NO_SANDBOX,
                policy,
                probe: ProbeReport {
                    backend: "none",
                    detail: "no sandbox requested".into(),
                },
            });
        }
        validate_workspace(&policy)?;
        let backend = platform_backend()?;
        let probe = backend.probe()?;
        Ok(Self {
            backend,
            policy,
            probe,
        })
    }

    /// Builds a sandbox over an explicit backend (used by tests to inject an unavailable or
    /// failing backend, and by callers that pick a backend themselves). Enforced modes probe;
    /// a probe failure is returned, never downgraded.
    pub fn from_backend(
        backend: &'static dyn SandboxBackend,
        mode: SandboxMode,
        workspace: impl AsRef<std::path::Path>,
    ) -> Result<Self, SandboxError> {
        // A non-enforced mode ignores the backend: nothing is wrapped, and the "none"
        // backend keeps that visible in `backend_name`.
        if !mode.is_enforced() {
            return Self::new(mode, workspace);
        }
        let policy = SandboxPolicy::new(mode, workspace)?;
        validate_workspace(&policy)?;
        let probe = backend.probe()?;
        Ok(Self {
            backend,
            policy,
            probe,
        })
    }

    pub fn mode(&self) -> SandboxMode {
        self.policy.mode
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Whether native git can write the repository metadata (`.git`) under this sandbox, with
    /// only the entries git can execute (`config`, `config.worktree`, `hooks/`) left read-only
    /// (ADR 0038 decision 4). This is the **effective** answer for this session: the policy may
    /// ask for [`GitAccess::WritableExceptExec`], but the backend must be able to enforce the
    /// carve-out. The shell tool's description and the startup line read this, so a session
    /// whose `.git` is still read-only (any `workspace` session on a backend without the
    /// carve-out) keeps the read-only wording.
    pub fn git_writable_except_exec(&self) -> bool {
        self.backend.git_writable_except_exec(&self.policy)
    }

    /// Whether outbound Unix socket connections are denied under this sandbox (ADR 0016
    /// revision 1). This is the **effective** answer for this session: the policy may list
    /// [`SandboxPolicy::allow_unix_sockets`], but only a backend that enforces the deny
    /// reports `true`. The `<runtime>` prompt layer reads this, so a session whose sockets
    /// stay reachable (any `workspace` session on a backend without the filter, Linux)
    /// never tells the model otherwise.
    pub fn restricts_unix_sockets(&self) -> bool {
        self.backend.restricts_unix_sockets(&self.policy)
    }

    pub fn probe(&self) -> &ProbeReport {
        &self.probe
    }

    /// A `Command` that runs `program args` under the sandbox. The returned command has its
    /// environment cleared to the policy's whitelist and its cwd set to the workspace, and
    /// owns any launcher fd (Linux `workspace`'s seccomp program) until it is dropped.
    pub fn command<S, I, A>(&self, program: S, args: I) -> Result<SandboxCommand, SandboxError>
    where
        S: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|a| a.as_ref().to_os_string())
            .collect();
        self.backend.command(&self.policy, program.as_ref(), &args)
    }

    /// Convenience over [`Sandbox::command`] for string arguments.
    pub fn command_str(
        &self,
        program: &str,
        args: &[&str],
    ) -> Result<SandboxCommand, SandboxError> {
        self.command(program, args.iter().copied())
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("mode", &self.policy.mode)
            .field("backend", &self.backend.name())
            .finish_non_exhaustive()
    }
}

/// Probes the platform backend without building a sandbox.
pub fn probe() -> Result<ProbeReport, SandboxError> {
    platform_backend()?.probe()
}

/// Applies the policy's environment whitelist to a command: clears everything, then sets the
/// named variables that exist in the parent. Shared by the backends.
#[allow(dead_code)] // used by the macOS backend; the Linux backend sets --setenv
pub(crate) fn apply_env(command: &mut Command, policy: &SandboxPolicy) {
    command.env_clear();
    for name in &policy.allow_env {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

/// The per-session temp directory, always inside the workspace so it is writable under
/// `workspace-write`. Created on demand.
pub(crate) fn workspace_tmp(policy: &SandboxPolicy) -> std::path::PathBuf {
    let tmp = policy.workspace.join(".radian-sandbox").join("tmp");
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// A fresh scratch directory for a backend probe. Shared by both backends.
pub(crate) fn probe_dir() -> Result<std::path::PathBuf, SandboxError> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("radian-sandbox-probe-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
