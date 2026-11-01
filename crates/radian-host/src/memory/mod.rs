//! The host-side memory layer (ADR 0021 D1/D2/D6/D7).
//!
//! [`project_identity_key`] turns a workspace into the git common dir (or its canonical path
//! outside a repository), and [`MemoryService`] is a thin layer over the file store and the
//! project registry for this workspace's project resolution. Both hosts call it; this module
//! never reads `HOME` for storage (the caller passes the memory root) and never touches the
//! network. [`open_path`] is the one place it starts a user's editor or the platform opener.
//!
//! [`injection`] reads the two `MEMORY.md` index files through the file store and refuses
//! memory writes while `/memory off` is in force; there is no model memory tool (ADR 0021 D7).

pub mod commands;
mod identity;
pub mod injection;
mod open;
mod service;

pub use commands::{
    COMMAND_NAMES, CommandError, CommandOutput, MemoryCommand, ProjectCommand, TYPE_NAMES,
    USAGE_ERROR, execute, parse_args, removed_message,
};
pub use identity::{candidate_name, project_identity_key};
pub use injection::{
    DEFAULT_INDEX_MAX_BYTES, DEFAULT_INDEX_MAX_LINES, InjectionConfig, MEMORY_SCOPES, MemoryHooks,
    MemoryInjection, Section,
};
pub use open::open_path;
pub use service::{
    CreatedProject, MemoryService, MemoryServiceError, MoveReport, ProjectForWrite, ProjectState,
    ScopeSelection, ScopedEntry, WriteScope,
};
