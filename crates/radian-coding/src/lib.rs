//! Built-in coding tools for radian: `read`, `write`, `patch`, `list`, `glob`, `grep`,
//! `git_status`, `git_diff`, and (only when the host supplies a sandbox) `shell`. [`AskTools`]
//! is the separate `ask_user` provider: a host with no workspace still mounts it (ADR 0024
//! Q5). See `README.md` next to this crate for the tool/limit tables.
//!
//! Every tool operates under one workspace root given by the host. The file tools enforce the
//! path boundary **here**, per call (see below); `shell` adds the OS sandbox as defence in
//! depth and is never run without one — see `src/shell.rs` for what the sandbox does and does
//! not enforce.
//!
//! - paths are workspace-relative; absolute paths and `..` escapes are refused;
//! - `read`, `grep`, `glob` and `list` additionally read two **read-only** roots outside the
//!   workspace (ADR 0039): the cargo registry sources, `$CARGO_HOME/registry/src`,
//!   and every toolchain's `$RUSTUP_HOME/toolchains/*/lib/rustlib/src`. A model names one as an
//!   absolute path or with `~`; it is opened from that root's fd with the same per-component
//!   `O_NOFOLLOW` walk, and a path under neither root is refused with a message naming them.
//!   `write`, `patch` and `shell`'s working directory never accept them;
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
//! - **`shell`'s confinement is exactly the sandbox's.** Under `workspace-write` the
//!   backend confines filesystem writes, sensitive reads and the network; the macOS backend
//!   leaves `mach-lookup` open, so the IPC boundary is not tightened, and no text may present
//!   the tool as full credential isolation (ADR 0012). `full-access` is **not** confined at
//!   all: it is the built-in default since ADR 0043, the tool still requires a caller-supplied
//!   sandbox, and its description says so. The tool's own guarantees are narrower and always
//!   hold: a command runs in its own
//!   session, which is killed on timeout, cancellation and leader exit (so a child that left
//!   its process group, like GNU `timeout`, is reached too); the host's
//!   environment is filtered to the sandbox whitelist in every mode; and each stream's output
//!   is read as a stream, retaining only a bounded head and a trimmed tail, so a command that
//!   prints gigabytes cannot grow this process's memory.

mod args;
mod ask;
mod git;
mod glob;
mod grep;
pub mod heavy;
pub mod host_git;
mod jobs;
mod list;
mod patch;
mod provider;
mod read;
mod read_roots;
pub mod safe_fs;
mod search;
mod shell;
mod workspace;
mod write;

pub use ask::{
    AskSession, AskTools, MAX_DESCRIPTION_CHARS, MAX_ID_CHARS, MAX_LABEL_CHARS, MAX_OPTIONS,
    MAX_PROMPT_CHARS, MAX_QUESTIONS, PromptError, Prompter, Question, QuestionAnswer, QuestionKind,
    QuestionOption, QuestionReply, QuestionRequest,
};
pub use heavy::{
    DEFAULT_HEAVY_PATTERNS, DEFAULT_HEAVY_SLOTS, HeavyCancelled, HeavyPermit, HeavySlots,
};
pub use jobs::{
    JOB_RING_BYTES, JobEvent, JobState, JobSummary, Jobs, MAX_RUNNING_JOBS, OUTPUT_HINT,
};
pub use provider::{CodingSession, CodingTools, CodingToolsError, Limits};
pub use read_roots::ReadRoots;
pub use shell::{UserBash, run_user_bash};
pub use workspace::{WorkspaceError, WorkspaceRoot};

use agent_core::ToolFailure;

/// The single error code for every workspace-boundary refusal, so a model (and a test) can
/// recognise "you tried to leave the workspace" without parsing prose.
pub(crate) const CODE_OUTSIDE: &str = "path_outside_workspace";

pub(crate) fn fail(code: &str, message: impl Into<String>) -> ToolFailure {
    ToolFailure::new(code, message)
}
