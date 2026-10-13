//! The memory data model (ADR 0021 D1/D4). A record is the replay result of the scope's
//! event log; ids are stable across revisions, exports and Purge.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A stable record id: generated once, kept across revisions, export/import and Purge.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(pub String);

impl MemoryId {
    /// A new id, unique within a process and across processes on the same machine (time +
    /// pid + a process-local counter, like the session id generator).
    pub fn generate() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(format!("mem-{nanos:x}-{:x}-{n:x}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

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

/// Where a record lives. v1 has Global and Project only (ADR 0021 D1); the representation is
/// tagged so a future scope does not break old records.
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

/// What kind of thing a memory is (ADR 0021 D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Preference,
    Decision,
    Fact,
    Lesson,
}

/// Where a record came from. The host decides this; the model never supplies it (ADR 0021
/// invariant 3). It is written once and never changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    User,
    Agent,
    Import,
}

/// The lifecycle dimension (ADR 0021 D4): `Active` or `Archived`. A forgotten record is not
/// in the state at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Archived,
}

/// Who confirmed a record. v1 only a user can (ADR 0021 D4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmer {
    User,
}

/// The confirmation dimension, independent of `Status` (ADR 0021 D4). Confirming does **not**
/// change `source`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    Unverified,
    Confirmed { by: Confirmer, at: String },
}

impl Verification {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Verification::Confirmed { .. })
    }
}

/// Where a record came from, for audit. Every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl Provenance {
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none()
            && self.turn_id.is_none()
            && self.file.is_none()
            && self.commit.is_none()
    }
}

/// One memory record, the replay result of its scope's log (ADR 0021 D4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: MemoryId,
    pub scope: Scope,
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub content: String,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Provenance::is_empty")]
    pub provenance: Provenance,
    pub verification: Verification,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    /// Starts at 1 and is incremented by every Revised/Archived/Restored/Confirmed.
    pub revision: u64,
}

impl MemoryRecord {
    /// A brand-new record: revision 1, `Active`, and `Confirmed` when a user wrote it
    /// (ADR 0021 D4: "用户自己写入的记录天然 Confirmed").
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MemoryId,
        scope: Scope,
        kind: Kind,
        key: Option<String>,
        content: String,
        source: Source,
        provenance: Provenance,
        at: String,
    ) -> Self {
        let verification = match source {
            Source::User => Verification::Confirmed {
                by: Confirmer::User,
                at: at.clone(),
            },
            Source::Agent | Source::Import => Verification::Unverified,
        };
        Self {
            id,
            scope,
            kind,
            key,
            content,
            source,
            provenance,
            verification,
            status: Status::Active,
            created_at: at.clone(),
            updated_at: at,
            revision: 1,
        }
    }

    /// The `updated_at`+`id` tiebreak used by the deterministic order (ADR 0021 D6).
    pub fn is_active(&self) -> bool {
        self.status == Status::Active
    }
}

/// The current time in the session/journal format (`2026-10-13T12:00:00.000000Z`).
pub fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}
