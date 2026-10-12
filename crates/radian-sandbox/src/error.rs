//! Why a sandbox could not be built or used.
//!
//! There is deliberately **no** "degrade to no sandbox" variant or fallback: a requested
//! enforcement mode either has a working backend or the construction fails. Callers must
//! refuse to run the shell/MCP in that mode (ADR 0012, decision 2).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// No sandbox backend exists for this operating system.
    #[error("sandbox: no backend for this platform ({0}); run is refused")]
    UnsupportedPlatform(&'static str),
    /// The backend's tool is missing or usable (e.g. `sandbox-exec` absent, `bwrap` cannot
    /// create a user namespace).
    #[error("sandbox: {backend} is not available: {detail}; run is refused (never unsandboxed)")]
    BackendUnavailable {
        backend: &'static str,
        detail: String,
    },
    /// The backend ran its probe but the probe proved the rules were not enforced (for
    /// example an SBPL operation name silently stopped matching after an OS update).
    #[error("sandbox: {backend} probe failed: {detail}; run is refused")]
    ProbeFailed {
        backend: &'static str,
        detail: String,
    },
    #[error("sandbox: invalid workspace {path}: {detail}")]
    InvalidWorkspace { path: PathBuf, detail: String },
    #[error("sandbox: io: {0}")]
    Io(#[from] std::io::Error),
}

/// The result of probing a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub backend: &'static str,
    /// A short human line about what the probe verified.
    pub detail: String,
}
