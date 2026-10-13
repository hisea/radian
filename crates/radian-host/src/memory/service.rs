//! The host-side memory service (ADR 0021 D1/D2/D8): one object that owns the storage layer,
//! the project registry and this workspace's project resolution, so every command goes through
//! the same scope, source and revision rules.
//!
//! The API is **synchronous**: the memory store is a single-writer JSONL log, each operation is
//! short (no network, no model), and the CLI and TUI both call it from a place where blocking on
//! an fsync is fine. An `async` facade would only wrap the same blocking calls.

use std::fmt;
use std::path::{Path, PathBuf};

use radian_memory::{
    Confirmer, ImportReport, Kind, MemoryError, MemoryId, MemoryRecord, MemoryStore, MoveReport,
    ProjectId, ProjectRegistry, Provenance, Scope, SearchQuery, Source, search,
};

use super::identity::project_identity_key;

/// Whether this workspace resolves to a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectState {
    /// The identity key is in the registry.
    Known(ProjectId),
    /// It is not. A Project-scoped operation fails; the user chooses `create_project` or
    /// [`MemoryService::relink`]. `candidates` lists the projects whose identity key has the
    /// same [`candidate_name`](super::identity::candidate_name) ("同名仓库"); the rule is
    /// deliberately a hint, not a match.
    Unlinked {
        identity_key: String,
        candidates: Vec<ProjectId>,
    },
}

/// Which scope(s) a read covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeSelection {
    /// Project then Global: the ADR 0021 D1 default. An unlinked workspace simply reads Global.
    #[default]
    Default,
    GlobalOnly,
    ProjectOnly,
}

/// Which scope a write (or a single-scope read like export/import) covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteScope {
    /// The current project. An unlinked workspace makes this an error, never a silent create.
    #[default]
    Project,
    Global,
}

/// A memory operation failure, with the host's own case: a workspace that is not linked to a
/// project.
#[derive(Debug)]
pub enum MemoryServiceError {
    Storage(MemoryError),
    Unlinked {
        identity_key: String,
        candidates: Vec<ProjectId>,
    },
}

impl fmt::Display for MemoryServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryServiceError::Storage(error) => write!(f, "{error}"),
            MemoryServiceError::Unlinked {
                identity_key,
                candidates,
            } => {
                write!(
                    f,
                    "this workspace is not linked to a project (identity key {identity_key}); "
                )?;
                if candidates.is_empty() {
                    write!(f, "no candidate projects")?;
                } else {
                    let list: Vec<&str> = candidates.iter().map(ProjectId::as_str).collect();
                    write!(f, "candidate projects: {}", list.join(", "))?;
                }
                write!(
                    f,
                    ". Run `agent memory relink <ProjectId>` or `agent memory project new`, \
                     or pass --global"
                )
            }
        }
    }
}

impl std::error::Error for MemoryServiceError {}

impl From<MemoryError> for MemoryServiceError {
    fn from(error: MemoryError) -> Self {
        MemoryServiceError::Storage(error)
    }
}

/// The memory service rooted at a memory directory, for one workspace.
#[derive(Debug, Clone)]
pub struct MemoryService {
    store: MemoryStore,
    registry: ProjectRegistry,
    workspace: PathBuf,
    identity_key: String,
}

impl MemoryService {
    /// Opens the service. The root is the memory directory (the host passes
    /// `radian_host::memory_dir()`); the workspace is canonicalized, and the identity key is
    /// derived from it. Nothing is created yet.
    pub fn open(workspace: &Path, root: impl Into<PathBuf>) -> Self {
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let identity_key = project_identity_key(&workspace)
            .to_string_lossy()
            .into_owned();
        let root = root.into();
        Self {
            store: MemoryStore::open(root.clone()),
            registry: ProjectRegistry::open(root),
            workspace,
            identity_key,
        }
    }

    pub fn store(&self) -> &MemoryStore {
        &self.store
    }

    pub fn registry(&self) -> &ProjectRegistry {
        &self.registry
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The identity key this workspace resolves by (ADR 0021 D2).
    pub fn identity_key(&self) -> &str {
        &self.identity_key
    }

    /// Whether the identity key is registered, and the candidates if it is not. Resolving
    /// never registers (ADR 0021 D2: no silent project identity).
    pub fn state(&self) -> Result<ProjectState, MemoryServiceError> {
        match self.registry.resolve(&self.identity_key)? {
            Some(project) => Ok(ProjectState::Known(project)),
            None => Ok(ProjectState::Unlinked {
                identity_key: self.identity_key.clone(),
                candidates: self.candidates()?,
            }),
        }
    }

    /// The current project, or the `Unlinked` error.
    pub fn project_id(&self) -> Result<ProjectId, MemoryServiceError> {
        match self.state()? {
            ProjectState::Known(project) => Ok(project),
            ProjectState::Unlinked {
                identity_key,
                candidates,
            } => Err(MemoryServiceError::Unlinked {
                identity_key,
                candidates,
            }),
        }
    }

    /// Registers this identity key as a new project. Only the explicit `agent memory project
    /// new` command calls it.
    pub fn create_project(&self) -> Result<ProjectId, MemoryServiceError> {
        Ok(self.registry.register(&self.identity_key)?)
    }

    /// Attaches this identity key to an existing project (ADR 0021 D2).
    pub fn relink(&self, project: &ProjectId) -> Result<(), MemoryServiceError> {
        Ok(self.registry.relink(&self.identity_key, project)?)
    }

    pub fn projects(&self) -> Result<Vec<ProjectId>, MemoryServiceError> {
        Ok(self.registry.projects()?)
    }

    pub fn identity_keys(&self, project: &ProjectId) -> Result<Vec<String>, MemoryServiceError> {
        Ok(self.registry.identity_keys(project)?)
    }

    /// Projects whose identity key is named like this workspace's ("同名仓库", ADR 0021 D2).
    fn candidates(&self) -> Result<Vec<ProjectId>, MemoryServiceError> {
        let current = super::identity::candidate_name(&self.identity_key);
        let mut candidates = Vec::new();
        for project in self.registry.projects()? {
            let named_alike = self
                .registry
                .identity_keys(&project)?
                .iter()
                .any(|key| super::identity::candidate_name(key) == current);
            if named_alike {
                candidates.push(project);
            }
        }
        Ok(candidates)
    }

    /// The scopes a read covers, in order. `Default` is Project then Global; Project is simply
    /// omitted when the workspace is unlinked, so Global reads always work.
    fn read_scopes(&self, selection: ScopeSelection) -> Result<Vec<Scope>, MemoryServiceError> {
        Ok(match selection {
            ScopeSelection::GlobalOnly => vec![Scope::Global],
            ScopeSelection::ProjectOnly => vec![Scope::Project(self.project_id()?)],
            ScopeSelection::Default => {
                let mut scopes = Vec::new();
                if let ProjectState::Known(project) = self.state()? {
                    scopes.push(Scope::Project(project));
                }
                scopes.push(Scope::Global);
                scopes
            }
        })
    }

    fn write_scope(&self, scope: WriteScope) -> Result<Scope, MemoryServiceError> {
        Ok(match scope {
            WriteScope::Global => Scope::Global,
            WriteScope::Project => Scope::Project(self.project_id()?),
        })
    }

    // ---- reads ----

    /// The combined, deduplicated view of the selected scopes (Project overrides Global,
    /// ADR 0021 D1). Archived records are included only when `include_archived`.
    pub fn list(
        &self,
        selection: ScopeSelection,
        include_archived: bool,
    ) -> Result<Vec<MemoryRecord>, MemoryServiceError> {
        let records = self.store.view(&self.read_scopes(selection)?)?;
        Ok(records
            .into_iter()
            .filter(|record| include_archived || record.is_active())
            .collect())
    }

    /// Keyword search over the selected scopes, in the deterministic ADR 0021 D6 order.
    pub fn search(
        &self,
        selection: ScopeSelection,
        query: &SearchQuery,
    ) -> Result<Vec<MemoryRecord>, MemoryServiceError> {
        let records = self.store.view(&self.read_scopes(selection)?)?;
        Ok(search(&records, query))
    }

    /// One record by id; the Project copy wins when both scopes hold it.
    pub fn get(
        &self,
        selection: ScopeSelection,
        id: &MemoryId,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let records = self.store.view(&self.read_scopes(selection)?)?;
        records
            .into_iter()
            .find(|record| record.id == *id)
            .ok_or_else(|| MemoryError::NotFound { id: id.clone() }.into())
    }

    // ---- writes. Every one is `Source::User` (ADR 0021 D7: user commands). ----

    pub fn add(
        &self,
        scope: WriteScope,
        kind: Kind,
        key: Option<String>,
        content: String,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.create(
            &scope,
            Source::User,
            kind,
            key,
            content,
            Provenance::default(),
        )?)
    }

    /// The agent's `memory_save` (ADR 0021 D5): `source: Agent`, and the store's key rules still
    /// apply — an Active user record with the key makes this a `KeyConflict`.
    pub fn save_as_agent(
        &self,
        scope: WriteScope,
        kind: Kind,
        key: Option<String>,
        content: String,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.create(
            &scope,
            Source::Agent,
            kind,
            key,
            content,
            Provenance::default(),
        )?)
    }

    /// The agent's `memory_revise` (ADR 0021 D5): the store refuses a `Source::User` record,
    /// so an agent can only change what an agent wrote. The user command uses [`Self::edit`].
    pub fn revise_as_agent(
        &self,
        scope: WriteScope,
        id: &MemoryId,
        expected_revision: u64,
        content: String,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self
            .store
            .revise(&scope, Source::Agent, id, expected_revision, content)?)
    }

    pub fn edit(
        &self,
        scope: WriteScope,
        id: &MemoryId,
        expected_revision: u64,
        content: String,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self
            .store
            .revise(&scope, Source::User, id, expected_revision, content)?)
    }

    pub fn archive(
        &self,
        scope: WriteScope,
        id: &MemoryId,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self
            .store
            .archive(&scope, Source::User, id, expected_revision)?)
    }

    pub fn restore(
        &self,
        scope: WriteScope,
        id: &MemoryId,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self
            .store
            .restore(&scope, Source::User, id, expected_revision)?)
    }

    pub fn forget(&self, scope: WriteScope, id: &MemoryId) -> Result<(), MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.forget(&scope, Source::User, id)?)
    }

    pub fn confirm(
        &self,
        scope: WriteScope,
        id: &MemoryId,
        expected_revision: u64,
    ) -> Result<MemoryRecord, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self
            .store
            .confirm(&scope, id, Confirmer::User, expected_revision)?)
    }

    pub fn purge(&self, scope: WriteScope) -> Result<usize, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.purge(&scope)?)
    }

    pub fn export(&self, scope: WriteScope) -> Result<String, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.export(&scope)?)
    }

    pub fn import(
        &self,
        scope: WriteScope,
        text: &str,
    ) -> Result<ImportReport, MemoryServiceError> {
        let scope = self.write_scope(scope)?;
        Ok(self.store.import(&scope, text)?)
    }

    /// `split`/`merge` name the two projects directly; they do not depend on this workspace's
    /// project.
    pub fn split(
        &self,
        from: &ProjectId,
        ids: &[MemoryId],
        to: &ProjectId,
    ) -> Result<MoveReport, MemoryServiceError> {
        Ok(self.store.split(from, ids, to)?)
    }

    pub fn merge(
        &self,
        into: &ProjectId,
        from: &ProjectId,
    ) -> Result<MoveReport, MemoryServiceError> {
        Ok(self.store.merge(into, from)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-memory-svc-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn service(label: &str) -> (PathBuf, PathBuf, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let root = dir.join("memory");
        let service = MemoryService::open(&ws, &root);
        (dir, ws, service)
    }

    /// A new workspace: the registry has no entry, so Project writes fail and **register
    /// nothing**; Global still works. This is the ADR 0021 D2 "no silent identity" rule.
    #[test]
    fn an_unlinked_workspace_refuses_a_project_write_and_still_reads_global() {
        let (dir, _ws, service) = service("unlinked");
        assert!(matches!(
            service.state().unwrap(),
            ProjectState::Unlinked { .. }
        ));
        // A Global add works.
        service
            .add(WriteScope::Global, Kind::Fact, None, "g".into())
            .unwrap();
        let global = service.list(ScopeSelection::Default, false).unwrap();
        assert_eq!(global.len(), 1);
        assert!(global[0].scope.is_global());

        // A Project add is refused, and no project directory appears.
        let error = service
            .add(WriteScope::Project, Kind::Fact, None, "p".into())
            .unwrap_err();
        assert!(
            matches!(error, MemoryServiceError::Unlinked { .. }),
            "{error}"
        );
        assert!(!service.store().root().join("projects").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_write_reads_back_after_creating_the_project() {
        let (dir, _ws, service) = service("create");
        let project = service.create_project().unwrap();
        assert_eq!(service.project_id().unwrap(), project);
        let record = service
            .add(
                WriteScope::Project,
                Kind::Fact,
                Some("k".into()),
                "p".into(),
            )
            .unwrap();
        assert_eq!(record.scope, Scope::Project(project));
        assert_eq!(record.source, Source::User);
        assert_eq!(
            service.list(ScopeSelection::ProjectOnly, false).unwrap(),
            vec![record.clone()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worktree (its `.git` file points at the main repository's common dir) resolves to the
    /// same project as the main repository, with no `git` process.
    #[test]
    fn a_worktree_resolves_to_the_main_repositories_project() {
        let dir = scratch("worktree");
        let main = dir.join("main");
        let common = main.join(".git");
        let gitdir = common.join("worktrees").join("wt");
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let wt = dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let pointer = std::path::PathBuf::from("..").join(gitdir.strip_prefix(&dir).unwrap());
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", pointer.display())).unwrap();
        let root = dir.join("memory");

        let main_service = MemoryService::open(&main, &root);
        let project = main_service.create_project().unwrap();
        main_service
            .add(WriteScope::Project, Kind::Fact, None, "shared".into())
            .unwrap();

        let wt_service = MemoryService::open(&wt, &root);
        assert_eq!(wt_service.project_id().unwrap(), project);
        assert_eq!(
            wt_service
                .list(ScopeSelection::ProjectOnly, false)
                .unwrap()
                .len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Moving a repository changes its identity key. The old project must be offered as a
    /// candidate (same repository name), and relinking must find the old records.
    #[test]
    fn relinking_after_a_move_finds_the_original_records() {
        let dir = scratch("relink");
        let old_ws = dir.join("here").join("radian");
        std::fs::create_dir_all(&old_ws).unwrap();
        let root = dir.join("memory");
        let before = MemoryService::open(&old_ws, &root);
        let project = before.create_project().unwrap();
        before
            .add(WriteScope::Project, Kind::Fact, None, "kept".into())
            .unwrap();

        let new_ws = dir.join("moved").join("radian");
        std::fs::create_dir_all(&new_ws).unwrap();
        let after = MemoryService::open(&new_ws, &root);
        let state = after.state().unwrap();
        let ProjectState::Unlinked { candidates, .. } = &state else {
            panic!("expected Unlinked, got {state:?}");
        };
        assert_eq!(candidates, &vec![project.clone()]);
        // No project write is possible until relinked.
        assert!(matches!(
            after.add(WriteScope::Project, Kind::Fact, None, "x".into()),
            Err(MemoryServiceError::Unlinked { .. })
        ));
        after.relink(&project).unwrap();
        assert_eq!(after.project_id().unwrap(), project);
        let records = after.list(ScopeSelection::ProjectOnly, false).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Global and a Project record with the same key: the Project one wins in the combined
    /// view (ADR 0021 D1), and the Project scope is listed before Global.
    #[test]
    fn a_project_record_overrides_a_global_one_with_the_same_key() {
        let (dir, _ws, service) = service("override");
        service.create_project().unwrap();
        service
            .add(
                WriteScope::Global,
                Kind::Fact,
                Some("k".into()),
                "global".into(),
            )
            .unwrap();
        service
            .add(
                WriteScope::Project,
                Kind::Fact,
                Some("k".into()),
                "project".into(),
            )
            .unwrap();
        let all = service.list(ScopeSelection::Default, false).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "project");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The archive/restore/confirm path keeps the source and bumps the revision.
    #[test]
    fn user_commands_set_the_user_source_and_check_the_revision() {
        let (dir, _ws, service) = service("revision");
        service.create_project().unwrap();
        let record = service
            .add(WriteScope::Project, Kind::Fact, None, "v1".into())
            .unwrap();
        assert_eq!(record.source, Source::User);
        assert!(record.verification.is_confirmed());
        let revised = service
            .edit(
                WriteScope::Project,
                &record.id,
                record.revision,
                "v2".into(),
            )
            .unwrap();
        assert_eq!(revised.revision, record.revision + 1);
        let error = service
            .edit(
                WriteScope::Project,
                &record.id,
                record.revision,
                "stale".into(),
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                MemoryServiceError::Storage(MemoryError::RevisionConflict { .. })
            ),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
