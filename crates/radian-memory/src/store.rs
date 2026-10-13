//! The memory store: Global and Project scope logs, CRUD with revision checks and the
//! source rules, Forget/Purge, split/merge, and the cross-scope view (ADR 0021 D1/D3/D4).
//!
//! Every operation is synchronous and holds the scope writer lock for its own short lifetime
//! (open → replay → mutate → fsync → drop); it never spans user interaction. Concurrent
//! writers therefore serialize, which is what ADR 0021 D4 asks of a store shared by many
//! sessions.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{MemoryError, PurgeStep};
use crate::event::MemoryEvent;
use crate::log::{Log, LoggedEvent};
use crate::model::{
    Confirmer, Kind, MemoryId, MemoryRecord, ProjectId, Provenance, Scope, Source, now_utc,
};

/// Where a split/merge calls its test seam.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovePhase {
    /// After the conflict check and before the target `Created`: the window a second writer
    /// must not be able to enter.
    BeforeTargetAppend,
    /// After the target `Created` and before the source `Forgotten`: a crash here leaves the
    /// half-finished on-disk state.
    AfterTargetAppend,
}

// Test seam: a callback run at a [`MovePhase`]; returning `true` aborts with
// [`MemoryError::SimulatedCrash`]. Thread-local, so a test never affects another one;
// production never sets it.
/// The move-hook callback type.
type MoveHook = Box<dyn FnMut(MovePhase) -> bool>;

thread_local! {
    static MOVE_HOOK: std::cell::RefCell<Option<MoveHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs the move-hook callback (this thread). Used only by tests.
#[doc(hidden)]
pub fn set_move_hook(hook: Box<dyn FnMut(MovePhase) -> bool>) {
    MOVE_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
}

fn run_move_hook(phase: MovePhase) -> bool {
    MOVE_HOOK.with(|slot| slot.borrow_mut().as_mut().is_some_and(|hook| hook(phase)))
}

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
        Self::from_events(path, log, events)
    }

    /// Opens an existing log for a read-only operation. `None` when it does not exist, so a
    /// read never creates a directory or an empty file.
    fn open_read(path: &Path) -> Result<Option<Self>, MemoryError> {
        match Log::open_existing(path, MemoryEvent::NAMES)? {
            Some((log, events)) => Ok(Some(Self::from_events(path, log, events)?)),
            None => Ok(None),
        }
    }

    fn from_events(path: &Path, log: Log, events: Vec<LoggedEvent>) -> Result<Self, MemoryError> {
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

    /// Opens (creating if needed) the scope's log for a write.
    fn open_scope(&self, scope: &Scope) -> Result<ScopeLog, MemoryError> {
        let path = self.log_path(scope);
        if let Some(parent) = path.parent() {
            crate::fs::ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
        }
        ScopeLog::open(&path)
    }

    /// Opens an existing scope's log for a read. `None` when it does not exist.
    fn read_scope(&self, scope: &Scope) -> Result<Option<ScopeLog>, MemoryError> {
        ScopeLog::open_read(&self.log_path(scope))
    }

    /// The current state of one scope, in id order. A scope that was never written reads as
    /// empty and is not created.
    pub fn records(&self, scope: &Scope) -> Result<Vec<MemoryRecord>, MemoryError> {
        Ok(self
            .read_scope(scope)?
            .map(|log| log.records.into_values().collect())
            .unwrap_or_default())
    }

    /// The combined view of several scopes. Two deterministic rules apply:
    ///
    /// - a Project Active record overrides a Global one with the same key (ADR 0021 D1);
    /// - the same id can appear in two scopes only after a half-finished move (a crash between
    ///   the target `Created` and the source `Forgotten`); the first scope in the caller's
    ///   order is kept. The host passes Project before Global, so the target wins.
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
        let mut seen = HashSet::new();
        all.retain(|record| seen.insert(record.id.clone()));
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
        guard_known(record)?;
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
        guard_known(record)?;
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
        guard_known(record)?;
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
            moved_to: None,
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
    /// block whose revision does not match is reported as a conflict; a block that changes an
    /// existing record's `key` or `kind` is reported as a rejection. Conflicts and rejections
    /// write nothing (ADR 0021 D3).
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
                    if let Some(field) = record.unknown_field() {
                        // This version cannot interpret the stored value, so it must not
                        // rewrite it. Only `kind` has a counterpart in the Markdown.
                        let imported = if field == "kind" {
                            item.kind.as_str().to_owned()
                        } else {
                            String::new()
                        };
                        report.rejections.push(ImportRejection {
                            id: item.id,
                            field: field.to_owned(),
                            stored: format!("<unknown {field}>"),
                            imported,
                        });
                        continue;
                    }
                    if record.key != item.key {
                        report.rejections.push(ImportRejection {
                            id: item.id,
                            field: "key".into(),
                            stored: record.key.clone().unwrap_or_default(),
                            imported: item.key.clone().unwrap_or_default(),
                        });
                        continue;
                    }
                    if record.kind != item.kind {
                        report.rejections.push(ImportRejection {
                            id: item.id,
                            field: "kind".into(),
                            stored: record.kind.as_str().to_owned(),
                            imported: item.kind.as_str().to_owned(),
                        });
                        continue;
                    }
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

    /// Moves the listed records from one project to another under **both** locks, taken in
    /// log-path order so two opposite splits cannot deadlock.
    ///
    /// The check and the two appends happen inside those locks: the target's key conflict is
    /// read and the target record written without a window, and the source record's revision
    /// cannot change between the read and the `Forgotten`. A record whose key is already
    /// Active in the target is left where it is and reported. A re-run after a crash between
    /// the two appends sees the target's copy and only completes the source's `Forgotten`
    /// (ADR 0021 D2).
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
        let (mut source, mut target) = self.open_pair(&from_scope, &to_scope)?;
        let mut report = MoveReport::default();
        for id in ids {
            let Some(record) = source.records.get(id).cloned() else {
                if target.records.contains_key(id) {
                    // The source `Forgotten` already landed in an earlier run.
                    report.moved.push(id.clone());
                } else {
                    report.missing.push(id.clone());
                }
                continue;
            };
            if let Some(existing) = target.records.get(id) {
                if existing.revision == record.revision && existing.content == record.content {
                    // A previous run wrote the target copy but crashed before the source
                    // `Forgotten`; complete the move.
                    source.append(&MemoryEvent::Forgotten {
                        id: id.clone(),
                        at: now_utc(),
                        moved_to: Some(to.clone()),
                    })?;
                    report.moved.push(id.clone());
                    continue;
                }
                report.conflicts.push(MoveConflict {
                    id: id.clone(),
                    key: record.key.clone().unwrap_or_default(),
                    held_by: existing.id.clone(),
                    held_revision: existing.revision,
                });
                continue;
            }
            if let Some(key) = record.key.as_deref()
                && let Some(existing) = target.active_by_key(key)
            {
                report.conflicts.push(MoveConflict {
                    id: id.clone(),
                    key: key.to_owned(),
                    held_by: existing.id.clone(),
                    held_revision: existing.revision,
                });
                continue;
            }
            let moved = MemoryRecord {
                scope: to_scope.clone(),
                ..record
            };
            if run_move_hook(MovePhase::BeforeTargetAppend) {
                return Err(MemoryError::SimulatedCrash);
            }
            target.append(&MemoryEvent::Created(moved))?;
            if run_move_hook(MovePhase::AfterTargetAppend) {
                return Err(MemoryError::SimulatedCrash);
            }
            source.append(&MemoryEvent::Forgotten {
                id: id.clone(),
                at: now_utc(),
                moved_to: Some(to.clone()),
            })?;
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

    /// Opens two scopes for a move, locking them in log-path order. The returned tuple is
    /// always `(a, b)`; only the acquisition order follows the paths.
    fn open_pair(&self, a: &Scope, b: &Scope) -> Result<(ScopeLog, ScopeLog), MemoryError> {
        let a_path = self.log_path(a);
        let b_path = self.log_path(b);
        for path in [&a_path, &b_path] {
            if let Some(parent) = path.parent() {
                crate::fs::ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
            }
        }
        if a_path <= b_path {
            Ok((ScopeLog::open(&a_path)?, ScopeLog::open(&b_path)?))
        } else {
            let second = ScopeLog::open(&b_path)?;
            let first = ScopeLog::open(&a_path)?;
            Ok((first, second))
        }
    }
}

fn guard_source(record: &MemoryRecord, actor: Source) -> Result<(), MemoryError> {
    if actor == Source::Agent && matches!(record.source, Source::User) {
        return Err(MemoryError::SourceProtected {
            id: record.id.clone(),
            actor,
            existing: record.source.clone(),
        });
    }
    Ok(())
}

/// A record with an unknown enum value is listed/exported/forgotten/purged but cannot be
/// changed by this version (ADR 0021 D4).
fn guard_known(record: &MemoryRecord) -> Result<(), MemoryError> {
    match record.unknown_field() {
        Some(field) => Err(MemoryError::UnknownRecordField {
            id: record.id.clone(),
            field,
        }),
        None => Ok(()),
    }
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
    /// Blocks refused because they changed a field an import may not change (or because the
    /// stored record carries an unknown value); nothing was written for them.
    pub rejections: Vec<ImportRejection>,
}

/// One imported block whose revision did not match the stored record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportConflict {
    pub id: MemoryId,
    pub expected: u64,
    pub found: u64,
}

/// One imported block refused because `field` differs from the stored record (`key`/`kind`),
/// or because the stored record has an unknown value this version may not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRejection {
    pub id: MemoryId,
    pub field: String,
    pub stored: String,
    pub imported: String,
}
