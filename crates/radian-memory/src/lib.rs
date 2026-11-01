//! `radian-memory`: the ADR 0021 memory storage layer.
//!
//! It is deliberately host-free: the caller passes the root directory (the host passes
//! `~/.radian/memory/`), and the crate never reads `HOME`. A record is one Markdown file with
//! YAML frontmatter; each scope has one `MEMORY.md` index. It provides:
//!
//! - [`FileStore`] — the record files and their indexes, with atomic writes and the name rule;
//! - [`migrate_log`] — the one-time conversion of an old `log.jsonl` (ADR 0021 D3);
//! - [`ProjectRegistry`] — the `projects.jsonl` identity-key → `ProjectId` registry.
//!
//! There is no model tool and no JSONL record store: writing a file is remembering and deleting
//! it is forgetting.

mod error;
mod files;
mod fs;
mod migrate;
mod model;
mod registry;

pub use error::MemoryError;
pub use files::{
    DESCRIPTION_MAX, FileProvenance, FileStore, Frontmatter, IndexLine, ListEntry, NAME_MAX,
    NewRecord, RECORD_SOURCES, RECORD_TYPES, Record, SESSIONS_INDEX_FILE, declared_source,
    index_lines, validate_record_name,
};
pub use migrate::migrate_log;
#[doc(hidden)]
pub use migrate::set_migration_crash_hook;
pub use model::{ProjectId, Scope, now_utc};
pub use registry::ProjectRegistry;
