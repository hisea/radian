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
//! - `token` (0600): the bearer token the HTTP API requires. It does not exist in this build.
//! - `hub.lock`: the flock a running hub holds for its lifetime, with its pid inside, the
//!   same shape as a repository's `runner.lock` (ADR 0048 decision 13). It does not exist in
//!   this build.
//!
//! The directory is on the sandbox deny list beside `credentials/` (ADR 0020): the model
//! never reads the registry or the token.

pub mod registry;

pub use registry::{
    HUB_LOCK, NAME_MAX_CHARS, REGISTRY_FILE, Registry, RepoEntry, TOKEN_FILE, valid_name,
};
