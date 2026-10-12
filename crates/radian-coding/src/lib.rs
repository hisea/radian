//! Built-in coding tools for radian: `read`, `write`, `patch`, `list`, `git_status`,
//! `git_diff`. **No shell, no command execution, read-only git.** See `README.md` next to
//! this crate for the tool/limit tables.
//!
//! Every tool operates under one workspace root given by the host. There is no sandbox
//! behind these tools (see `docs/adr/0012-permission-sources.md` and
//! `docs/spikes/macos-sandbox.md`: the sandbox, when one exists, can read the whole
//! `$HOME` anyway), so the path boundary is enforced **here**, per call:
//!
//! - paths are workspace-relative; absolute paths and `..` escapes are refused;
//! - the target is canonicalized and must stay under the canonical workspace root, which
//!   catches symlink escapes, including a symlinked parent directory;
//! - `write`/`patch` never follow a symlink at the destination (`O_NOFOLLOW` on Unix) and
//!   refuse a hard-linked target (`st_nlink > 1`) because writing it could change a file
//!   visible outside the workspace.
//!
//! # Known limits (stated, not papered over)
//!
//! - **Hard links cannot be read-protected.** A pre-existing hard link inside the
//!   workspace to a file outside it has an in-workspace canonical path, so `read` cannot
//!   tell where the other link lives. Hard links are detectable only for `write`/`patch`
//!   (nlink > 1), and only because refusing them is safe; this crate does not claim reads
//!   are confined when a hard link is involved. The tools themselves cannot create links,
//!   so such a link must already exist.
//! - **TOCTOU is only partly mitigated.** The final component of a write is opened with
//!   `O_NOFOLLOW`, so replacing it with a symlink between the check and the open fails the
//!   open. A parent directory swapped for a symlink between canonicalization and the open
//!   is *not* caught; that window is not closed in this slice.
//! - **Sensitive file contents are not filtered.** Reading a file that contains a secret
//!   puts that secret into the tool result and therefore into the session JSONL (ADR 0006
//!   §11). Filtering is not this crate's job; hosts should treat session files as
//!   potentially sensitive.
//! - **`git` is invoked as a child process.** It can read `~/.gitconfig` and the repository,
//!   and it is told not to take optional locks (`--no-optional-locks`); the tests assert
//!   `.git` is unchanged. No sandbox is applied to it.

mod git;
mod list;
mod patch;
mod provider;
mod read;
mod shell;
mod workspace;
mod write;

pub use provider::{CodingSession, CodingTools, CodingToolsError, Limits};
pub use workspace::WorkspaceError;

use agent_core::ToolFailure;

/// The single error code for every workspace-boundary refusal, so a model (and a test) can
/// recognise "you tried to leave the workspace" without parsing prose.
pub(crate) const CODE_OUTSIDE: &str = "path_outside_workspace";

pub(crate) fn fail(code: &str, message: impl Into<String>) -> ToolFailure {
    ToolFailure::new(code, message)
}
