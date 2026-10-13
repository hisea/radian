//! `radian-memory`: the ADR 0021 M1 memory storage layer.
//!
//! It is deliberately host-free: the caller passes the root directory (the host will pass
//! `~/.radian/memory/`), and the crate never reads `HOME`. It provides:
//!
//! - [`MemoryStore`] — Global and Project scope logs with CRUD, revision checks, the source
//!   rules (an agent cannot touch a user record), Forget/Purge, split/merge primitives, and
//!   the cross-scope view;
//! - [`ProjectRegistry`] — the `projects.jsonl` identity-key → `ProjectId` registry;
//! - [`search`] — keyword filtering and the deterministic order (ADR 0021 D6);
//! - [`export`]/[`parse`] — the Markdown view.
//!
//! The log mechanics (single-writer flock, per-append fsync, torn-tail truncation, the
//! critical/unknown-event policy) mirror `radian-session` (ADR 0006 §3.2/§7) without
//! depending on it: the event schema and replay here are the memory's own (ADR 0021 D3).

mod error;
mod event;
mod fs;
mod log;
mod markdown;
mod model;
mod registry;
mod search;
mod store;

pub use error::{MemoryError, PurgeStep};
pub use event::MemoryEvent;
#[doc(hidden)]
pub use log::set_open_hook;
pub use markdown::{ParsedRecord, export, parse};
pub use model::{
    Confirmer, Kind, MemoryId, MemoryRecord, ProjectId, Provenance, Scope, Source, Status,
    Verification, now_utc,
};
pub use registry::ProjectRegistry;
pub use search::{SearchQuery, ranked, search};
pub use store::{
    ImportConflict, ImportRejection, ImportReport, MemoryStore, MoveConflict, MoveReport,
};
#[doc(hidden)]
pub use store::{MovePhase, set_move_hook};
