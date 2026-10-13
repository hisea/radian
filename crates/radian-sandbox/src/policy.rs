//! The sandbox policy: a product-level mode plus the concrete paths a backend needs.
//!
//! [`SandboxMode`] is the product concept (ADR 0012 P1); it is not defined by SBPL or by
//! bubblewrap flags. The backend translates a policy into its own mechanism.

use std::path::{Path, PathBuf};

use crate::error::SandboxError;

/// The execution boundary a caller asked for. `FullAccess` is the explicit "no sandbox"
/// choice; a backend is never picked for it. Nothing here silently turns an enforced mode
/// into `FullAccess`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Read-only: the workspace is not writable, and no network unless requested.
    Safe,
    /// The workspace is writable; repository metadata and sensitive directories are not. The
    /// network is **on** (ADR 0016): outbound and local-loopback connect/bind/listen are
    /// allowed, because an agent that cannot run the project's own tests is not useful.
    WorkspaceWrite,
    /// Explicitly unsandboxed. The user chose this; the runtime never selects it.
    FullAccess,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::WorkspaceWrite => "workspace",
            Self::FullAccess => "full",
        }
    }

    /// Whether this mode requires an enforcing backend.
    pub fn is_enforced(self) -> bool {
        matches!(self, Self::Safe | Self::WorkspaceWrite)
    }
}

/// Everything a backend needs. Paths are canonicalized when the policy is built, because
/// SBPL `subpath` and bwrap binds match resolved paths (the macOS spike's realpath lesson).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    /// Canonical workspace root. The only writable place in `workspace-write`.
    pub workspace: PathBuf,
    /// `true` allows network; the default follows the mode: **off for `safe`, on for
    /// `workspace`** (ADR 0016). A caller can still override it with [`SandboxPolicy::network`].
    pub network: bool,
    /// Paths inside (or outside) the workspace that must stay read-only, e.g. `.git`.
    pub read_only: Vec<PathBuf>,
    /// Paths that must not be readable (ADR 0012 decision 3). Missing paths are
    /// dropped at build time; a backend never widens them.
    pub deny_read: Vec<PathBuf>,
    /// Environment variable names passed through to the child. Everything else is dropped.
    pub allow_env: Vec<String>,
}

/// Environment variables passed through by default. Deliberately small; a caller adds what
/// its tools need. Nothing else (no API keys, no tokens) reaches a sandboxed child unless
/// the caller named it.
pub const DEFAULT_ALLOW_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Directories that are readable only if the user names them, by default (ADR 0012 owner
/// decision 3). Relative to `$HOME`. This list is a default, not a complete credential
/// boundary: the macOS backend leaves `mach-lookup` open, so IPC is not contained (ADR
/// 0012 P4/P6) and no user-facing text may claim full credential isolation.
pub const DEFAULT_DENY_READ: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".netrc",
    ".npmrc",
    ".config/gcloud",
    ".docker/config.json",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Safari",
    "Library/Cookies",
];

impl SandboxPolicy {
    /// A `workspace` policy: canonical workspace, discovered git metadata read-only,
    /// default sensitive paths denied, **network on** (ADR 0016), default environment.
    pub fn workspace_write(workspace: impl AsRef<Path>) -> Result<Self, SandboxError> {
        Self::new(SandboxMode::WorkspaceWrite, workspace)
    }

    /// A `safe` (read-only, network off) policy.
    pub fn safe(workspace: impl AsRef<Path>) -> Result<Self, SandboxError> {
        Self::new(SandboxMode::Safe, workspace)
    }

    pub fn new(mode: SandboxMode, workspace: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let workspace = canonical_dir(workspace.as_ref())?;
        let read_only = discover_git_metadata(&workspace);
        let deny_read = default_deny_read();
        Ok(Self {
            mode,
            workspace,
            // ADR 0016: the workspace tier opens the network; safe keeps it off.
            network: matches!(mode, SandboxMode::WorkspaceWrite),
            read_only,
            deny_read,
            allow_env: DEFAULT_ALLOW_ENV.iter().map(|s| (*s).to_owned()).collect(),
        })
    }

    pub fn network(mut self, on: bool) -> Self {
        self.network = on;
        self
    }

    pub fn allow_env(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        if !self.allow_env.contains(&name) {
            self.allow_env.push(name);
        }
        self
    }

    /// Adds a read-only carve-out (a directory gets `subpath`, a file `literal`).
    pub fn read_only(mut self, path: impl AsRef<Path>) -> Self {
        if let Ok(p) = std::fs::canonicalize(path.as_ref())
            && !self.read_only.contains(&p)
        {
            self.read_only.push(p);
        }
        self
    }

    /// Adds a path that must not be readable. Unlike [`SandboxPolicy::read_only`] the path
    /// may not exist yet; an existing ancestor is used so the rule still binds.
    pub fn deny_read(mut self, path: impl AsRef<Path>) -> Self {
        if let Some(p) = canonicalize_existing_prefix(path.as_ref())
            && !self.deny_read.contains(&p)
        {
            self.deny_read.push(p);
        }
        self
    }
}

/// Canonicalizes a directory or fails with `InvalidWorkspace`.
pub(crate) fn canonical_dir(path: &Path) -> Result<PathBuf, SandboxError> {
    let meta = std::fs::metadata(path).map_err(|e| SandboxError::InvalidWorkspace {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    if !meta.is_dir() {
        return Err(SandboxError::InvalidWorkspace {
            path: path.to_path_buf(),
            detail: "not a directory".into(),
        });
    }
    std::fs::canonicalize(path).map_err(|e| SandboxError::InvalidWorkspace {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })
}

/// Canonicalizes `path`, or its nearest existing ancestor when the path itself is absent, so
/// a deny rule for `~/.ssh` still binds before the directory has been created.
pub(crate) fn canonicalize_existing_prefix(path: &Path) -> Option<PathBuf> {
    // Resolve symlinks in the part that exists and keep the missing tail: a deny rule for a
    // not-yet-created `~/.netrc` must name `~/.netrc`, never stop at `$HOME` (which would
    // deny the whole home directory).
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut resolved = canonical;
            for part in tail.iter().rev() {
                resolved.push(part);
            }
            return Some(resolved);
        }
        let name = existing.file_name()?.to_os_string();
        tail.push(name);
        if !existing.pop() {
            return None;
        }
    }
}

/// The default sensitive paths that exist on this machine, canonicalized, plus their
/// `$HOME` prefix so a deny rule still covers a directory that appears later.
fn default_deny_read() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rel in DEFAULT_DENY_READ {
        let p = home.join(rel);
        // Keep only entries we can resolve (existing) or whose parent resolves (so the
        // rule stands even if the sensitive file is created later).
        if let Some(resolved) = canonicalize_existing_prefix(&p) {
            out.push(resolved);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Finds `.git` entries under the workspace (bounded depth) and returns the paths that must
/// stay read-only: the git directory itself, and for a worktree pointer file also the
/// shared common directory it names.
pub(crate) fn discover_git_metadata(workspace: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_git(workspace, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

const MAX_GIT_DEPTH: usize = 3;

fn collect_git(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_GIT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".git" {
            add_git_entry(&path, out);
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() && name != ".radian" {
            collect_git(&path, depth + 1, out);
        }
    }
}

fn add_git_entry(path: &Path, out: &mut Vec<PathBuf>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        if let Ok(p) = std::fs::canonicalize(path) {
            out.push(p);
        }
        return;
    }
    // A worktree's `.git` is a file: `gitdir: <path>`. Keep the pointer and the shared
    // common directory (the grandparent of `.../.git/worktrees/<name>`) read-only.
    if let Ok(canonical) = std::fs::canonicalize(path) {
        out.push(canonical);
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let Some(gitdir) = text.trim().strip_prefix("gitdir:") else {
        return;
    };
    let gitdir = PathBuf::from(gitdir.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        path.parent().map(|p| p.join(&gitdir)).unwrap_or(gitdir)
    };
    if let Ok(gitdir) = std::fs::canonicalize(&gitdir) {
        out.push(gitdir.clone());
        // `<common>/worktrees/<name>` → `<common>`.
        if let Some(common) = gitdir.parent().and_then(Path::parent) {
            out.push(common.to_path_buf());
        }
    }
}
