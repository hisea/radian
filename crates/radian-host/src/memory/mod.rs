//! The host-side memory layer (ADR 0021 D1/D2/D8).
//!
//! [`project_identity_key`] turns a workspace into the git common dir (or its canonical path
//! outside a repository), and [`MemoryService`] is one object that owns the storage layer, the
//! project registry and this workspace's project resolution. Both hosts call it; this module
//! never reads `HOME` (the caller passes the memory root), never touches the network, and never
//! starts a process.

mod identity;
pub mod injection;
mod service;
pub mod tools;

pub use identity::{candidate_name, project_identity_key};
pub use injection::{Injection, InjectionConfig, MemoryHooks, plan};
pub use service::{MemoryService, MemoryServiceError, ProjectState, ScopeSelection, WriteScope};
pub use tools::{
    DEFAULT_SEARCH_LIMIT, LOW_TRUST_NOTE, MAX_SEARCH_LIMIT, MemorySession, MemoryTools,
};
