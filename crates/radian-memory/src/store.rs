//! The memory store: Global and Project scope logs, CRUD with revision checks and the
//! source rules, Forget/Purge, and the cross-scope view (ADR 0021 D1/D3/D4).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{MemoryError, PurgeStep};
use crate::event::MemoryEvent;
use crate::log::Log;
use crate::model::{
    Confirmer, Kind, MemoryId, MemoryRecord, ProjectId, Provenance, Scope, Source, now_utc,
};

/// The in-memory replay of one scope's log.
struct ScopeLog {
    path: PathBuf,
    log: Log,
    records: BTreeMap<MemoryId, MemoryRecord>,
    forgotten: BTreeSet<MemoryId>,
}

impl ScopeLog {
    fn open(path: &Path) -> Result<Self, MemoryError> {
        let (log, events) = Log::open(path, MemoryEvent::NAMES)?;
        let mut records = BTreeMap::new();
        let mut forgotten = BTreeSet::new();
        for accepted in &events {
            let event = MemoryEvent::from_data(&accepted.event, &accepted.data).map_err(|e| {
                MemoryError::Corrupt {
                    path: path.to_path_buf(),
                    line: accepted.line,
                    column: 0,
                    message: e.to_string(),
                }
            })?;
            event
                .apply(&mut records, &mut forgotten)
                .map_err(|message| MemoryError::Corrupt {
                    path: path.to_path_buf(),
                    line: accepted.line,
                    column: 0,
                    message,
                })?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            log,
            records,
            forgotten,
        })
    }

    fn append(&mut self, event: &MemoryEvent) -> Result<(), MemoryError> {
        self.log
            .append(event.name(), event.critical(), &event.to_data())?;
        event
            .apply(&mut self.records, &mut self.forgotten)
            .map_err(|message| MemoryError::Corrupt {
                path: self.path.clone(),
                line: 0,
                column: 0,
                message,
            })
    }

    fn active_by_key(&self, key: &str) -> Option<&MemoryRecord> {
        self.records
            .values()
            .find(|record| record.is_active() && record.key.as_deref() == Some(key))
    }
}

/// A store rooted at one directory. The caller passes the root (the host passes
/// `~/.radian/memory/`); this crate never reads `HOME`.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    root: PathBuf,
}

impl MemoryStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The log path for a scope.
    pub fn log_path(&self, scope: &Scope) -> PathBuf {
        match scope {
            Scope::Global => self.root.join("global").join("log.jsonl"),
            Scope::Project(project) => self
                .root
                .join("projects")
                .join(project.as_str())
                .join("log.jsonl"),
        }
    }

    fn open_scope(&self, scope: &Scope) -> Result<ScopeLog, MemoryError> {
        if scope.is_global() {
            ScopeLog::open(&self.log_path(scope))
        } else {
            // A project log creates its directory so a first write may target a new project.
            let path = self.log_path(scope);
            if let Some(parent) = path.parent() {
                crate::fs::ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
            }
            ScopeLog::open(&path)
        }
    }

    /// The current state of one scope, in id order.
    pub fn records(&self, scope: &Scope) -> Result<Vec<MemoryRecord>, MemoryError> {
        let log = self.open_scope(scope)?;
        Ok(log.records.into_values().collect())
    }

    /// The combined view of several scopes: a Project Active record overrides a Global one
    /// with the same key (ADR 0021 D1).
    pub fn view(&self, scopes: &[Scope]) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut all = Vec::new();
        for scope in scopes {
            all.extend(self.records(scope)?);
        }
        let project_keys: HashSet<String> = all
            .iter()
            .filter(|record| !record.scope.is_global() && record.is_active())
            .filter_map(|record| record.key.clone())
            .collect();
        all.retain(|record| {
            !(record.scope.is_global()
                && record.is_active()
                && record
                    .key
                    .as_ref()
                    .is_some_and(|key| project_keys.contains(key)))
        });
        Ok(all)
    }

    /// Creates a record. Any Active record already holding `key` in the scope makes this a
    /// [`MemoryError::KeyConflict`] — including a user record an agent is trying to shadow
    /// (ADR 0021 D4/D5).
    pub fn create(
        &self,
        scope: &Scope,
        source: Source,
        kind: Kind,
        key: Option<String>,
        content: impl Into<String>,
        provenance: Provenance,
    ) -> Result<MemoryRecord, MemoryError> {
        self.create_with_id(
            scope,
            MemoryId::generate(),
            source,
            kind,
            key,
            content,
            provenance,
        )
    }

    /// Creates a record with a caller-supplied id. The import path uses this so the id stays
    /// stable across export/import (ADR 0021 D3).
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_id(
        &self,
        scope: &Scope,
        id: MemoryId,
        source: Source,
        kind: Kind,
        key: Option<String>,
        content: impl Into<String>,
        provenance: Provenance,
    ) -> Result<MemoryRecord, MemoryError> {
        let mut log = self.open_scope(scope)?;
        if log.records.contains_key(&id) {
            return Err(MemoryError::Envelope {
                path: self.log_path(scope),
                line: 0,
                message: format!("record {id} already exists"),
            });
        }
        if let Some(key) = &key
            && let Some(existing) = log.active_by_key(key)
        {
            return Err(MemoryError::KeyConflict {
                key: key.clone(),
                id: existing.id.clone(),
                revision: existing.revision,
            });
        }
        let record = MemoryRecord::new(
            id,
            scope.clone(),
            kind,
            key,
            content.into(),
            source,
            provenance,
            now_utc(),
        );
        log.append(&MemoryEvent::Created(record.clone()))?;
        Ok(record)
    }

    /// Revises a record's content. `actor` is the caller's source, checked against the
    /// record's own source (ADR 0021 D5).
    pub fn revise(
        &self,
        scope: &Scope,
        actor: Source,
        id: &MemoryId,
        expected_revision: u64,
        content: impl Into<String>,
    ) -> Result<MemoryRecord, MemoryError> {
        let mut log = self.open_scope(scope)?;
        let record = log
            .records
            .get(id)
            .ok_or_else(|| MemoryError::NotFound { id: id.clone() })?;
        guard_source(record, actor)?;
        check_revision(record, expected_revision)?;
        let revision = record.revision + 1;
        log.append(&MemoryEvent::Revised {
            id: id.clone(),
            revision,
            content: content.into(),
            at: now_utc(),
        })?;
        Ok(log.records.get(id).expect("just appended").clone())
    }

    pub fn archive(
        &self,
        scope: &Scope,
        actor: Source,
        id: &MemoryId,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryError> {
        self.set_status(scope, actor, id, expected_revision, false)
    }

    pub fn restore(
        &self,
        scope: &Scope,
        actor: Source,
        id: &MemoryId,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryError> {
        self.set_status(scope, actor, id, expected_revision, true)
    }

    fn set_status(
        &self,
        scope: &Scope,
        actor: Source,
        id: &MemoryId,
        expected_revision: u64,
        active: bool,
    ) -> Result<MemoryRecord, MemoryError> {
        let mut log = self.open_scope(scope)?;
        let record = log
            .records
            .get(id)
            .ok_or_else(|| MemoryError::NotFound { id: id.clone() })?;
        guard_source(record, actor)?;
        check_revision(record, expected_revision)?;
        let revision = record.revision + 1;
        let event = if active {
            MemoryEvent::Restored {
                id: id.clone(),
                revision,
                at: now_utc(),
            }
        } else {
            MemoryEvent::Archived {
                id: id.clone(),
                revision,
                at: now_utc(),
            }
        };
        log.append(&event)?;
        Ok(log.records.get(id).expect("just appended").clone())
    }

    /// Confirms a record. The source is preserved; only `verification` changes (ADR 0021 D4).
    pub fn confirm(
        &self,
        scope: &Scope,
        id: &MemoryId,
        by: Confirmer,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryError> {
        let mut log = self.open_scope(scope)?;
        let record = log
            .records
            .get(id)
            .ok_or_else(|| MemoryError::NotFound { id: id.clone() })?;
        check_revision(record, expected_revision)?;
        let revision = record.revision + 1;
        log.append(&MemoryEvent::Confirmed {
            id: id.clone(),
            revision,
            by,
            at: now_utc(),
        })?;
        Ok(log.records.get(id).expect("just appended").clone())
    }

    /// Logical delete: the record disappears from every view and never comes back on replay
    /// (ADR 0021 D3).
    pub fn forget(&self, scope: &Scope, actor: Source, id: &MemoryId) -> Result<(), MemoryError> {
        let mut log = self.open_scope(scope)?;
        let record = log
            .records
            .get(id)
            .ok_or_else(|| MemoryError::NotFound { id: id.clone() })?;
        guard_source(record, actor)?;
        log.append(&MemoryEvent::Forgotten {
            id: id.clone(),
            at: now_utc(),
        })?;
        Ok(())
    }

    /// Physically rewrites the scope's log so the forgotten records' content is gone
    /// (ADR 0021 D3). Returns the number of records removed.
    pub fn purge(&self, scope: &Scope) -> Result<usize, MemoryError> {
        self.purge_with_fault(scope, None)
    }

    /// [`Self::purge`] with a crash point for the atomicity test (ADR 0021 D3).
    pub fn purge_with_fault(
        &self,
        scope: &Scope,
        fault: Option<PurgeStep>,
    ) -> Result<usize, MemoryError> {
        let mut log = self.open_scope(scope)?;
        let removed: Vec<MemoryId> = log.forgotten.iter().cloned().collect();
        let mut lines: Vec<(String, bool, serde_json::Value)> = Vec::new();
        for record in log.records.values() {
            let event = MemoryEvent::Created(record.clone());
            lines.push((event.name().to_string(), event.critical(), event.to_data()));
        }
        let event = MemoryEvent::Purged {
            removed: removed.clone(),
            at: now_utc(),
        };
        lines.push((event.name().to_string(), event.critical(), event.to_data()));
        log.log.purge(&lines, fault)?;
        Ok(removed.len())
    }

    /// Renders one scope as Markdown.
    pub fn export(&self, scope: &Scope) -> Result<String, MemoryError> {
        Ok(crate::markdown::export(&self.records(scope)?))
    }

    /// Imports a Markdown export. A block whose `id` and `revision` match an existing record
    /// becomes a revision; a block with a new id becomes a record with `source: Import`; a
    /// block whose revision does not match is reported and not written (ADR 0021 D3).
    pub fn import(&self, scope: &Scope, text: &str) -> Result<ImportReport, MemoryError> {
        let parsed = crate::markdown::parse(text)?;
        let mut report = ImportReport::default();
        for item in parsed {
            let existing = self
                .records(scope)?
                .into_iter()
                .find(|record| record.id == item.id);
            match existing {
                Some(record) => {
                    if record.revision != item.revision {
                        report.conflicts.push(ImportConflict {
                            id: item.id,
                            expected: item.revision,
                            found: record.revision,
                        });
                        continue;
                    }
                    self.revise(scope, Source::Import, &item.id, item.revision, item.content)?;
                    report.revised.push(item.id);
                }
                None => {
                    self.create_with_id(
                        scope,
                        item.id.clone(),
                        Source::Import,
                        item.kind,
                        item.key,
                        item.content,
                        Provenance::default(),
                    )?;
                    report.created.push(item.id);
                }
            }
        }
        Ok(report)
    }

    /// Moves the listed records from one project to another. A record whose key is already
    /// Active in the target is left where it is and reported (ADR 0021 D2).
    pub fn split(
        &self,
        from: &ProjectId,
        ids: &[MemoryId],
        to: &ProjectId,
    ) -> Result<MoveReport, MemoryError> {
        if from == to {
            return Err(MemoryError::Envelope {
                path: self.root.clone(),
                line: 0,
                message: "split needs two different projects".into(),
            });
        }
        let from_scope = Scope::Project(from.clone());
        let to_scope = Scope::Project(to.clone());
        let mut report = MoveReport::default();
        for id in ids {
            let record = {
                let source = self.open_scope(&from_scope)?;
                source.records.get(id).cloned()
            };
            let Some(record) = record else {
                report.missing.push(id.clone());
                continue;
            };
            let conflict = {
                let target = self.open_scope(&to_scope)?;
                record
                    .key
                    .as_deref()
                    .and_then(|key| target.active_by_key(key))
                    .map(|existing| MoveConflict {
                        id: id.clone(),
                        key: record.key.clone().unwrap_or_default(),
                        held_by: existing.id.clone(),
                        held_revision: existing.revision,
                    })
            };
            if let Some(conflict) = conflict {
                report.conflicts.push(conflict);
                continue;
            }
            let moved = MemoryRecord {
                scope: to_scope.clone(),
                ..record
            };
            {
                let mut target = self.open_scope(&to_scope)?;
                target.append(&MemoryEvent::Created(moved))?;
            }
            {
                let mut source = self.open_scope(&from_scope)?;
                source.append(&MemoryEvent::Forgotten {
                    id: id.clone(),
                    at: now_utc(),
                })?;
            }
            report.moved.push(id.clone());
        }
        Ok(report)
    }

    /// Merges `from` into `into`, reporting a conflict for each key both scopes hold.
    pub fn merge(&self, into: &ProjectId, from: &ProjectId) -> Result<MoveReport, MemoryError> {
        let ids: Vec<MemoryId> = self
            .records(&Scope::Project(from.clone()))?
            .into_iter()
            .map(|record| record.id)
            .collect();
        self.split(from, &ids, into)
    }
}

fn guard_source(record: &MemoryRecord, actor: Source) -> Result<(), MemoryError> {
    if actor == Source::Agent && record.source == Source::User {
        return Err(MemoryError::SourceProtected {
            id: record.id.clone(),
            actor,
            existing: record.source,
        });
    }
    Ok(())
}

fn check_revision(record: &MemoryRecord, expected: u64) -> Result<(), MemoryError> {
    if record.revision != expected {
        return Err(MemoryError::RevisionConflict {
            id: record.id.clone(),
            expected,
            found: record.revision,
        });
    }
    Ok(())
}

/// What a split/merge moved and what it could not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveReport {
    pub moved: Vec<MemoryId>,
    pub conflicts: Vec<MoveConflict>,
    pub missing: Vec<MemoryId>,
}

/// One record a split/merge could not move because the target holds its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveConflict {
    pub id: MemoryId,
    pub key: String,
    pub held_by: MemoryId,
    pub held_revision: u64,
}

/// What an import did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub created: Vec<MemoryId>,
    pub revised: Vec<MemoryId>,
    pub conflicts: Vec<ImportConflict>,
}

/// One imported block whose revision did not match the stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportConflict {
    pub id: MemoryId,
    pub expected: u64,
    pub found: u64,
}
