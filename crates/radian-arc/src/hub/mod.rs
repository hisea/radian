//! The hub: one process that manages every repository registered on this machine.
//!
//! The hub keeps one directory, `$RADIAN_HOME/hub/` (0700), with three files:
//!
//! - `repos.toml` (0600): the repository registry this module reads and writes. It is a TOML
//!   document of `[[repos]]` tables, each with a `name` (1 to 48 characters from `a-z`, `0-9`
//!   and `-`, unique) and an absolute, canonical `path` (unique) to an arc repository.
//!   `radian hub add`, `remove` and `list` write and read it; a running hub re-reads it every
//!   interval and never writes it. Every write goes through a temporary file in the same
//!   directory and an atomic rename, so a reader sees either the old or the new whole
//!   document, never a partial one.
//! - `token` (0600): the bearer token the HTTP API requires, 32 random bytes as 64 hex
//!   characters. [`token::ensure`] creates it on the first `radian hub` start or for `radian
//!   hub pair`; [`token::rotate`] replaces it atomically.
//! - `hub.lock`: the flock a running hub holds for its lifetime, with its pid inside, the
//!   same shape as a repository's `runner.lock` (ADR 0048 decisions 13 and 16). [`http::serve`]
//!   takes it before it binds or schedules anything. The TUI probes it take-and-release and
//!   never creates it.
//!
//! The directory is on the sandbox deny list beside `credentials/` (ADR 0020): the model
//! never reads the registry or the token.
//!
//! The scheduling loop itself is [`r#loop`](crate::hub::r#loop): one loop per registered
//! repository on its own OS thread, the registry re-read every interval, and the machine-wide
//! cap.
//!
//! [`http`] is the loopback HTTP API: every request carries the bearer token, and the read
//! routes return the same bytes `radian arc list --json` and `radian arc show --json` print.

pub mod http;
pub mod lock;
pub mod r#loop;
pub mod registry;
pub mod status;
pub mod token;

pub use lock::HubLock;
pub use r#loop::run;
pub use registry::{
    HUB_LOCK, NAME_MAX_CHARS, REGISTRY_FILE, Registry, RepoEntry, TOKEN_FILE, valid_name,
};
pub use status::{HubStatus, RepoState, RepoStatus};
