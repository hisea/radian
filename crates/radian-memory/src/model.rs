//! The small identity model the file store and the registry need (ADR 0021 D1/D2/D4).
//!
//! A record's identity is its scope and its name, and a project's identity is the stable
//! `ProjectId` the registry maps an identity key to. The old JSONL store's record, revision and
//! lifecycle types are gone; the migration reader keeps the shapes it needs in `migrate.rs`.

use std::fmt;

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// A stable project id. The registry maps an identity key (a git common dir or a canonical
/// workspace path) to one of these; the same worktree shares it (ADR 0021 D2).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

impl ProjectId {
    pub fn generate() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(format!("prj-{nanos:x}-{:x}-{n:x}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a record lives. Only Global and Project exist (ADR 0021 D1); the representation is
/// tagged so a future scope does not break old files.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    Project(ProjectId),
}

impl Scope {
    pub fn is_global(&self) -> bool {
        matches!(self, Scope::Global)
    }
}

/// Now, in the RFC 3339 UTC form every timestamp uses.
pub fn now_utc() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}
