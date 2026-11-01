//! The one-time migration of an old JSONL memory log (ADR 0021 D3).
//!
//! The old store's log is gone, but a user's machine may still hold one. This module keeps the
//! only JSONL reader the file store still needs: it replays `log.jsonl` with the old event
//! schema and writes every Active record as one Markdown file, then renames the log to
//! `log.jsonl.migrated`. Nothing else reads the old event types.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::MemoryError;
use crate::files::{
    DESCRIPTION_MAX, Frontmatter, NAME_MAX, fsync_dir, name_fits, write_index_dir, write_record,
};

const SCHEMA_VERSION: u64 = 1;
const LOG_FILE: &str = "log.jsonl";
const MIGRATED_LOG_FILE: &str = "log.jsonl.migrated";

/// Event names the old reader knew. `StorePurged` is audit only.
const EVENT_NAMES: &[&str] = &[
    "MemoryCreated",
    "MemoryRevised",
    "MemoryArchived",
    "MemoryRestored",
    "MemoryConfirmed",
    "MemoryForgotten",
    "StorePurged",
];

thread_local! {
    static CRASH_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

/// Installs a one-shot crash hook, before the log rename. Used only by the migration test so
/// an interrupted run can be replayed.
#[doc(hidden)]
pub fn set_migration_crash_hook(hook: Box<dyn FnOnce()>) {
    CRASH_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
}

fn run_crash_hook() -> bool {
    CRASH_HOOK.with(|slot| match slot.borrow_mut().take() {
        Some(hook) => {
            hook();
            true
        }
        None => false,
    })
}

/// The old record shape, reduced to what the migration maps.
#[derive(Debug, Clone, Deserialize)]
struct OldRecord {
    id: String,
    kind: String,
    #[serde(default)]
    key: Option<String>,
    content: String,
    source: String,
    #[serde(default)]
    provenance: OldProvenance,
    #[serde(default)]
    status: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OldProvenance {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
}

impl OldRecord {
    fn is_active(&self) -> bool {
        self.status.is_empty() || self.status == "active"
    }
}

#[derive(Debug, Deserialize)]
struct CreatedData {
    record: OldRecord,
}

#[derive(Debug, Deserialize)]
struct RevisedData {
    id: String,
    content: String,
    at: String,
}

#[derive(Debug, Deserialize)]
struct StatusData {
    id: String,
    at: String,
}

#[derive(Debug, Deserialize)]
struct ForgottenData {
    id: String,
    #[allow(dead_code)]
    at: String,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    schema_version: u64,
    seq: u64,
    #[allow(dead_code)]
    ts: String,
    event: String,
    critical: bool,
    data: Value,
}

fn corrupt(path: &Path, line: u64, message: impl Into<String>) -> MemoryError {
    MemoryError::Corrupt {
        path: path.to_path_buf(),
        line,
        column: 0,
        message: message.into(),
    }
}

fn parse_envelope(raw: &str, path: &Path, line: u64) -> Result<Envelope, MemoryError> {
    serde_json::from_str(raw).map_err(|e| MemoryError::Corrupt {
        path: path.to_path_buf(),
        line,
        column: e.column(),
        message: e.to_string(),
    })
}

fn from_event<T: serde::de::DeserializeOwned>(
    event: &Envelope,
    path: &Path,
    line: u64,
    name: &str,
) -> Result<T, MemoryError> {
    serde_json::from_value(event.data.clone())
        .map_err(|e| corrupt(path, line, format!("{name}: {e}")))
}

fn apply_event(
    event: &Envelope,
    records: &mut BTreeMap<String, OldRecord>,
    path: &Path,
    line: u64,
) -> Result<(), MemoryError> {
    match event.event.as_str() {
        "MemoryCreated" => {
            let created: CreatedData = from_event(event, path, line, "MemoryCreated")?;
            records.insert(created.record.id.clone(), created.record);
        }
        "MemoryRevised" => {
            let revised: RevisedData = from_event(event, path, line, "MemoryRevised")?;
            let record = records
                .get_mut(&revised.id)
                .ok_or_else(|| corrupt(path, line, "revised before it existed"))?;
            record.content = revised.content;
            record.updated_at = revised.at;
        }
        "MemoryArchived" => {
            let status: StatusData = from_event(event, path, line, "MemoryArchived")?;
            let record = records
                .get_mut(&status.id)
                .ok_or_else(|| corrupt(path, line, "archived before it existed"))?;
            record.status = "archived".to_owned();
            record.updated_at = status.at;
        }
        "MemoryRestored" => {
            let status: StatusData = from_event(event, path, line, "MemoryRestored")?;
            let record = records
                .get_mut(&status.id)
                .ok_or_else(|| corrupt(path, line, "restored before it existed"))?;
            record.status = "active".to_owned();
            record.updated_at = status.at;
        }
        // Confirmation never changes a file's contents; the file store has no such field.
        "MemoryConfirmed" => {}
        "MemoryForgotten" => {
            let forgotten: ForgottenData = from_event(event, path, line, "MemoryForgotten")?;
            records.remove(&forgotten.id);
        }
        "StorePurged" => {}
        other => {
            if event.critical {
                return Err(MemoryError::UnknownCritical {
                    path: path.to_path_buf(),
                    line,
                    event: other.to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// Converts one scope's old `log.jsonl` into files and an index, once. A missing log or an
/// existing `log.jsonl.migrated` is a no-op; a crash before the rename leaves the log, and the
/// next call writes the same bytes again (the writes are idempotent).
pub fn migrate_log(scope_dir: impl AsRef<Path>) -> Result<(), MemoryError> {
    let dir = scope_dir.as_ref();
    let log_path = dir.join(LOG_FILE);
    let migrated = dir.join(MIGRATED_LOG_FILE);
    if migrated.exists() || !log_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&log_path).map_err(|e| MemoryError::io(&log_path, e))?;

    let mut records: BTreeMap<String, OldRecord> = BTreeMap::new();
    let mut schema: Option<u64> = None;
    let lines: Vec<&str> = text.lines().collect();
    let committed = if text.ends_with('\n') {
        lines.len()
    } else {
        lines.len().saturating_sub(1)
    };
    for (index, raw) in lines.iter().take(committed).enumerate() {
        let line = index as u64 + 1;
        let event = parse_envelope(raw, &log_path, line)?;
        match schema {
            None => {
                if event.schema_version != SCHEMA_VERSION {
                    return Err(MemoryError::Envelope {
                        path: log_path.clone(),
                        line,
                        message: format!(
                            "unsupported schema_version {} (expected {SCHEMA_VERSION})",
                            event.schema_version
                        ),
                    });
                }
                schema = Some(event.schema_version);
            }
            Some(version) if event.schema_version != version => {
                return Err(MemoryError::Envelope {
                    path: log_path.clone(),
                    line,
                    message: "schema_version changed mid-log".into(),
                });
            }
            Some(_) => {}
        }
        if event.seq != index as u64 {
            return Err(MemoryError::BadSeq {
                path: log_path.clone(),
                line,
                expected: index as u64,
                found: event.seq,
            });
        }
        if !EVENT_NAMES.contains(&event.event.as_str()) && event.critical {
            return Err(MemoryError::UnknownCritical {
                path: log_path.clone(),
                line,
                event: event.event,
            });
        }
        if EVENT_NAMES.contains(&event.event.as_str()) {
            apply_event(&event, &mut records, &log_path, line)?;
        }
    }

    let mut taken = BTreeSet::new();
    let mut index = String::new();
    for record in records.values() {
        if !record.is_active() {
            continue;
        }
        let name = unique_name(&base_name(record), &mut taken);
        let description = description_of(&record.content);
        let frontmatter = Frontmatter {
            name: name.clone(),
            description: description.clone(),
            record_type: type_for_kind(&record.kind).to_owned(),
            source: source_for_source(&record.source),
            created: record.created_at.clone(),
            modified: record.updated_at.clone(),
            last_used: None,
            provenance: provenance_of(record),
        };
        write_record(dir, &frontmatter, &record.content)?;
        index.push_str(&format!("- [{name}]({name}.md) — {description}\n"));
    }
    write_index_dir(dir, &index)?;

    if run_crash_hook() {
        return Err(MemoryError::SimulatedCrash);
    }
    std::fs::rename(&log_path, &migrated).map_err(|e| MemoryError::io(&migrated, e))?;
    fsync_dir(dir)
}

fn base_name(record: &OldRecord) -> String {
    if let Some(key) = &record.key
        && name_fits(key)
    {
        return key.clone();
    }
    slug_of(&record.content)
}

fn description_of(content: &str) -> String {
    content
        .split(['\n', '\r'])
        .next()
        .unwrap_or("")
        .chars()
        .take(DESCRIPTION_MAX)
        .collect()
}

fn slug_of(content: &str) -> String {
    let mut slug = String::new();
    for word in content.split_whitespace().take(8) {
        if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        for ch in word.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
                slug.push(lower);
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
    }
    let mut slug = slug.trim_matches('-').to_owned();
    if slug.chars().count() > NAME_MAX {
        slug = slug.chars().take(NAME_MAX).collect();
        slug = slug.trim_end_matches('-').to_owned();
    }
    if slug.is_empty() {
        slug.push_str("memory");
    }
    slug
}

fn unique_name(base: &str, taken: &mut BTreeSet<String>) -> String {
    let base = if base.is_empty() { "memory" } else { base };
    if taken.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut n = 2u64;
    loop {
        let suffix = format!("-{n}");
        let max = NAME_MAX.saturating_sub(suffix.chars().count());
        let mut stem: String = base.chars().take(max).collect();
        while stem.ends_with('-') {
            stem.pop();
        }
        if stem.is_empty() {
            stem.push_str("memory");
        }
        let candidate = format!("{stem}{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn type_for_kind(kind: &str) -> &'static str {
    match kind {
        "preference" => "user",
        // An unknown old kind has no new counterpart; `project` is the general factual bucket.
        "lesson" => "feedback",
        _ => "project",
    }
}

fn source_for_source(source: &str) -> String {
    match source {
        "user" => "user".to_owned(),
        "agent" => "model".to_owned(),
        "import" => "import".to_owned(),
        // A value a newer old writer defined is kept when it is already a new source.
        raw if crate::files::RECORD_SOURCES.contains(&raw) => raw.to_owned(),
        _ => "import".to_owned(),
    }
}

fn provenance_of(record: &OldRecord) -> Option<crate::files::FileProvenance> {
    if record.provenance.session_id.is_none() && record.provenance.turn_id.is_none() {
        return None;
    }
    Some(crate::files::FileProvenance {
        session_id: record.provenance.session_id.clone(),
        turn_id: record.provenance.turn_id.clone(),
    })
}
