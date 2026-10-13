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
mod error;
mod policy;
#[cfg(target_os = "macos")]
mod seatbelt;

#[cfg(feature = "testing")]
pub mod harness;

use std::ffi::{OsStr, OsString};
use std::process::Command;

pub use error::{ProbeReport, SandboxError};
pub use policy::{DEFAULT_ALLOW_ENV, DEFAULT_DENY_READ, SandboxMode, SandboxPolicy};

/// A concrete sandbox mechanism. `command` returns a launcher `Command` that runs `program`
/// with `args` under the policy; the caller may add cwd/stdio but must not widen the
/// environment (the returned command already cleared it).
pub trait SandboxBackend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Runs a probe that proves the mechanism actually denies something. Returning `Ok` is
    /// the only way a backend may be used for an enforced mode.
    fn probe(&self) -> Result<ProbeReport, SandboxError>;

    /// Builds the launcher command. `program`/`args` are the child's argv; the backend
    /// prepends its own launcher and options.
    fn command(
        &self,
        policy: &SandboxPolicy,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<Command, SandboxError>;
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
    ) -> Result<Command, SandboxError> {
        let mut command = Command::new(program);
        command.args(args).current_dir(&policy.workspace);
        Ok(command)
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
/// construction fails.
pub struct Sandbox {
    backend: &'static dyn SandboxBackend,
    policy: SandboxPolicy,
    probe: ProbeReport,
}

impl Sandbox {
    /// Builds a sandbox for `mode` and `workspace`.
    ///
    /// `Safe`/`WorkspaceWrite` require a working backend: if the tool is missing or the probe
    /// fails, this returns an error and the caller must refuse to run. `FullAccess` returns a
    /// transparent sandbox without probing, because nothing is being enforced.
    pub fn new(
        mode: SandboxMode,
        workspace: impl AsRef<std::path::Path>,
    ) -> Result<Self, SandboxError> {
        let policy = SandboxPolicy::new(mode, workspace)?;
        Self::from_policy(policy)
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

    pub fn probe(&self) -> &ProbeReport {
        &self.probe
    }

    /// A `Command` that runs `program args` under the sandbox. The returned command has its
    /// environment cleared to the policy's whitelist and its cwd set to the workspace.
    pub fn command<S, I, A>(&self, program: S, args: I) -> Result<Command, SandboxError>
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
    pub fn command_str(&self, program: &str, args: &[&str]) -> Result<Command, SandboxError> {
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
