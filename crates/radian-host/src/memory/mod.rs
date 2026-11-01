//! The host-side memory layer (ADR 0021 D1/D2/D6).
//!
//! [`project_identity_key`] turns a workspace into the git common dir (or its canonical path
//! outside a repository), and [`MemoryService`] is one object that owns the storage layer, the
//! project registry and this workspace's project resolution. Both hosts call it; this module
//! never reads `HOME` (the caller passes the memory root), never touches the network, and never
//! starts a process.
//!
//! [`injection`] reads the two `MEMORY.md` index files through the file store; there is no model
//! memory tool (ADR 0021 D7).

pub mod commands;
mod guard;
mod identity;
pub mod injection;
mod service;
mod touch;

pub use commands::{
    CommandError, CommandOutput, MemoryCommand, ProjectCommand, USAGE_ERROR, execute, parse_args,
};
pub use guard::{
    INPUT_GUARD, LONG_TOKEN, MemoryWriteGuard, SECRET_GUARD, SettledCall, journal_has_mcp_result,
    settled_calls,
};
pub use identity::{candidate_name, project_identity_key};
pub use injection::{
    DEFAULT_INDEX_MAX_BYTES, DEFAULT_INDEX_MAX_LINES, InjectionConfig, MEMORY_SCOPES, MemoryHooks,
    MemoryInjection, Section,
};
pub use service::{
    CreatedProject, MemoryService, MemoryServiceError, ProjectForWrite, ProjectState,
    ScopeSelection, WriteScope,
};
pub use touch::MemoryTouch;
