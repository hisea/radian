//! The host-side memory service (ADR 0021 D1/D2/D8): a thin layer over the file store and the
//! project registry, so every command uses the same scope, name and index rules.
//!
//! The API is synchronous: the store is local files, each operation is short, and the CLI and
//! TUI both call it where blocking on an fsync is fine.

use std::fmt;
use std::path::{Path, PathBuf};

use radian_memory::{
    FileStore, ListEntry, MemoryError, NewRecord, ProjectId, ProjectRegistry, Record, Scope,
    validate_record_name,
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

/// Which scope a write or a single-scope read covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteScope {
    /// The current project. An unlinked workspace makes this an error, never a silent create.
    #[default]
    Project,
    Global,
}

/// What [`MemoryService::project_for_write`] resolved for a write in the default scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectForWrite {
    /// The identity key was already registered: the write goes to this project.
    Linked(ProjectId),
    /// The identity key comes from a git repository and was not registered: it was registered
    /// now, and the host shows [`CreatedProject::notice`].
    Created(CreatedProject),
}

/// A project a default-scope write just registered for this workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedProject {
    pub project: ProjectId,
    pub identity_key: String,
    pub candidates: Vec<ProjectId>,
}

impl CreatedProject {
    /// The one notice line the hosts show.
    pub fn notice(&self) -> String {
        let mut line = format!(
            "created project {} for identity key {} (project memory goes there now)",
            self.project, self.identity_key
        );
        if let Some(previous) = self.candidates.first() {
            let list: Vec<&str> = self.candidates.iter().map(ProjectId::as_str).collect();
            line.push_str(&format!(
                "; a previous project ({}) already had this repository name — run `radian \
                 memory relink {previous}` to attach this workspace to it, or `radian memory \
                 merge {} {previous}` to move its files into the new project",
                list.join(", "),
                self.project,
            ));
        }
        line
    }
}

/// One `list` entry with its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedEntry {
    Record {
        scope: Scope,
        frontmatter: radian_memory::Frontmatter,
    },
    Unreadable {
        scope: Scope,
        path: PathBuf,
        reason: String,
    },
}

impl ScopedEntry {
    pub fn scope(&self) -> &Scope {
        match self {
            ScopedEntry::Record { scope, .. } | ScopedEntry::Unreadable { scope, .. } => scope,
        }
    }
}

/// What `split`/`merge` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveReport {
    pub moved: Vec<String>,
    pub missing: Vec<String>,
    pub conflicts: Vec<String>,
    /// `(old name, new name)` for a merge collision.
    pub renamed: Vec<(String, String)>,
}

/// A memory operation failure, with the host's own cases.
#[derive(Debug)]
pub enum MemoryServiceError {
    Storage(MemoryError),
    Unlinked {
        identity_key: String,
        candidates: Vec<ProjectId>,
    },
    /// This directory's identity key does not come from a git repository.
    NotAProjectWorkspace {
        identity_key: String,
    },
    /// No file with this name in the selected scopes.
    NotFound {
        name: String,
    },
    /// A file with this name already exists in the selected scope.
    AlreadyExists {
        name: String,
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
                    ". Run `radian memory relink <ProjectId>` or `radian memory project new`"
                )
            }
            MemoryServiceError::NotAProjectWorkspace { identity_key } => write!(
                f,
                "this directory is not a project workspace: its identity key ({identity_key}) \
                 does not come from a git repository, so the memory was not saved. Run \
                 `radian memory project new` to register this directory as a project"
            ),
            MemoryServiceError::NotFound { name } => write!(f, "no memory file named {name:?}"),
            MemoryServiceError::AlreadyExists { name } => {
                write!(f, "a memory file named {name:?} already exists")
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
    files: FileStore,
    registry: ProjectRegistry,
    workspace: PathBuf,
    identity_key: String,
    git_identity: bool,
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
        let git_identity = super::identity::is_git_identity(&workspace);
        let root = root.into();
        Self {
            files: FileStore::open(root.clone()),
            registry: ProjectRegistry::open(root),
            workspace,
            identity_key,
            git_identity,
        }
    }

    pub fn files(&self) -> &FileStore {
        &self.files
    }

    pub fn registry(&self) -> &ProjectRegistry {
        &self.registry
    }

    pub fn root(&self) -> &Path {
        self.files.root()
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The identity key this workspace resolves by (ADR 0021 D2).
    pub fn identity_key(&self) -> &str {
        &self.identity_key
    }

    /// Whether the identity key is registered, and the candidates if it is not.
    pub fn state(&self) -> Result<ProjectState, MemoryServiceError> {
        match self.registry.resolve(&self.identity_key)? {
            Some(project) => Ok(ProjectState::Known(project)),
            None => Ok(ProjectState::Unlinked {
                identity_key: self.identity_key.clone(),
                candidates: self.candidates()?,
            }),
        }
    }

    /// The memory scope directories the file tools may read **and write** (ADR 0039 decision
    /// 4): `<memory root>/global/` and, when this workspace is linked to a project,
    /// `<memory root>/projects/<ProjectId>/`. Each directory is created 0700 on first use.
    pub fn file_tool_roots(&self) -> Vec<PathBuf> {
        let mut scopes = vec![Scope::Global];
        if let Ok(project) = self.project_id() {
            scopes.push(Scope::Project(project));
        }
        scopes
            .iter()
            .map(|scope| {
                let dir = self.files.scope_dir(scope);
                let _ = crate::ensure_private_dir(&dir);
                dir
            })
            .collect()
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

    /// Registers this identity key as a new project.
    pub fn create_project(&self) -> Result<ProjectId, MemoryServiceError> {
        Ok(self.registry.register(&self.identity_key)?)
    }

    /// Resolves the Project scope for a **write** in the default scope (ADR 0021).
    pub fn project_for_write(&self) -> Result<ProjectForWrite, MemoryServiceError> {
        match self.state()? {
            ProjectState::Known(project) => Ok(ProjectForWrite::Linked(project)),
            ProjectState::Unlinked {
                identity_key,
                candidates,
            } => {
                if !self.git_identity {
                    return Err(MemoryServiceError::NotAProjectWorkspace { identity_key });
                }
                let project = self.registry.register(&identity_key)?;
                Ok(ProjectForWrite::Created(CreatedProject {
                    project,
                    identity_key,
                    candidates,
                }))
            }
        }
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
    /// omitted when the workspace is unlinked.
    pub fn read_scopes(&self, selection: ScopeSelection) -> Result<Vec<Scope>, MemoryServiceError> {
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

    /// The scope a write goes to.
    pub fn write_scope(&self, scope: WriteScope) -> Result<Scope, MemoryServiceError> {
        Ok(match scope {
            WriteScope::Global => Scope::Global,
            WriteScope::Project => Scope::Project(self.project_id()?),
        })
    }

    /// The scope directory a command prints or opens.
    pub fn scope_dir(&self, scope: &Scope) -> PathBuf {
        self.files.scope_dir(scope)
    }

    pub fn record_path(&self, scope: &Scope, name: &str) -> PathBuf {
        self.files.record_path(scope, name)
    }

    // ---- reads ----

    /// Every file in the selected scopes, project first then global, readable files first.
    pub fn list(&self, selection: ScopeSelection) -> Result<Vec<ScopedEntry>, MemoryServiceError> {
        let mut entries = Vec::new();
        for scope in self.read_scopes(selection)? {
            for entry in self.files.list(&scope)? {
                entries.push(match entry {
                    ListEntry::Record(frontmatter) => ScopedEntry::Record {
                        scope: scope.clone(),
                        frontmatter,
                    },
                    ListEntry::Unreadable { path, reason } => ScopedEntry::Unreadable {
                        scope: scope.clone(),
                        path,
                        reason,
                    },
                });
            }
        }
        Ok(entries)
    }

    /// The first selected scope that holds `name`, project first.
    pub fn locate_name(
        &self,
        selection: ScopeSelection,
        name: &str,
    ) -> Result<Option<Scope>, MemoryServiceError> {
        validate_record_name(name).map_err(|reason| MemoryError::InvalidName {
            name: name.to_owned(),
            reason,
        })?;
        for scope in self.read_scopes(selection)? {
            if self.files.exists(&scope, name)? {
                return Ok(Some(scope));
            }
        }
        Ok(None)
    }

    /// One parsed record.
    pub fn read(&self, scope: &Scope, name: &str) -> Result<Record, MemoryServiceError> {
        Ok(self.files.read(scope, name)?)
    }

    /// The file's bytes as stored.
    pub fn read_text(&self, scope: &Scope, name: &str) -> Result<String, MemoryServiceError> {
        let path = self.files.record_path(scope, name);
        Ok(std::fs::read_to_string(&path).map_err(|source| MemoryError::Open { path, source })?)
    }

    // ---- writes ----

    /// Creates `<name>.md` with an empty body and a `source: user` skeleton, then appends its
    /// index line. The name is validated and an existing file refused before anything is written.
    pub fn add(
        &self,
        scope: WriteScope,
        name: &str,
        record_type: &str,
    ) -> Result<(Scope, Record), MemoryServiceError> {
        validate_record_name(name).map_err(|reason| MemoryError::InvalidName {
            name: name.to_owned(),
            reason,
        })?;
        let scope = self.write_scope(scope)?;
        if self.files.exists(&scope, name)? {
            return Err(MemoryServiceError::AlreadyExists {
                name: name.to_owned(),
            });
        }
        let record = self.files.write(
            &scope,
            NewRecord {
                name: name.to_owned(),
                description: String::new(),
                record_type: record_type.to_owned(),
                source: "user".to_owned(),
                provenance: None,
                body: String::new(),
            },
        )?;
        self.append_index_line(&scope, name, "")?;
        Ok((scope, record))
    }

    /// Deletes `<name>.md` and its index line. Deleting the file is forgetting.
    pub fn remove(&self, scope: &Scope, name: &str) -> Result<(), MemoryServiceError> {
        if !self.files.exists(scope, name)? {
            return Err(MemoryServiceError::NotFound {
                name: name.to_owned(),
            });
        }
        self.files.delete(scope, name)?;
        self.remove_index_file(scope, &format!("{name}.md"))?;
        Ok(())
    }

    /// `split`: move each named file and its index line from one project to another. A missing
    /// source name is reported; a name already present in the target is reported and left in
    /// place (never overwritten).
    pub fn split(
        &self,
        from: &ProjectId,
        to: &ProjectId,
        names: &[String],
    ) -> Result<MoveReport, MemoryServiceError> {
        let source = Scope::Project(from.clone());
        let target = Scope::Project(to.clone());
        let mut report = MoveReport::default();
        for name in names {
            let mut from_path = self.files.record_path(&source, name);
            let to_path = self.files.record_path(&target, name);
            if !from_path.is_file() {
                report.missing.push(name.clone());
                continue;
            }
            if to_path.exists() {
                report.conflicts.push(name.clone());
                continue;
            }
            crate::ensure_private_dir(&self.files.scope_dir(&target)).map_err(|source| {
                MemoryError::Open {
                    path: self.files.scope_dir(&target),
                    source,
                }
            })?;
            std::fs::rename(&from_path, &to_path).map_err(|source| MemoryError::Open {
                path: from_path.clone(),
                source,
            })?;
            self.move_index_line(&source, &target, name, name)?;
            report.moved.push(name.clone());
            let _ = &mut from_path;
        }
        Ok(report)
    }

    /// `merge`: move every file from `from` into `into`. A name already present in the target
    /// is reported, and the `from` file is renamed `<name>-2`, `<name>-3`, ... first.
    pub fn merge(
        &self,
        into: &ProjectId,
        from: &ProjectId,
    ) -> Result<MoveReport, MemoryServiceError> {
        let source = Scope::Project(from.clone());
        let target = Scope::Project(into.clone());
        let mut report = MoveReport::default();
        let mut taken: std::collections::BTreeSet<String> = self
            .files
            .list(&target)?
            .into_iter()
            .map(|entry| match entry {
                ListEntry::Record(frontmatter) => frontmatter.name,
                ListEntry::Unreadable { path, .. } => path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            })
            .collect();
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.files.scope_dir(&source)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if file_name == "MEMORY.md" || !file_name.ends_with(".md") {
                    continue;
                }
                names.push(file_name.trim_end_matches(".md").to_owned());
            }
        }
        names.sort();
        for name in names {
            let mut from_path = self.files.record_path(&source, &name);
            let mut target_name = name.clone();
            if taken.contains(&target_name) {
                target_name = unique_name(&name, &mut taken);
                self.files
                    .rename(&source, &name, &target_name)
                    .map_err(MemoryServiceError::from)?;
                from_path = self.files.record_path(&source, &target_name);
                report.conflicts.push(name.clone());
                report.renamed.push((name.clone(), target_name.clone()));
            } else {
                taken.insert(target_name.clone());
            }
            let to_path = self.files.record_path(&target, &target_name);
            crate::ensure_private_dir(&self.files.scope_dir(&target)).map_err(|source| {
                MemoryError::Open {
                    path: self.files.scope_dir(&target),
                    source,
                }
            })?;
            std::fs::rename(&from_path, &to_path).map_err(|source| MemoryError::Open {
                path: from_path.clone(),
                source,
            })?;
            self.move_index_line(&source, &target, &name, &target_name)?;
            report.moved.push(target_name);
        }
        Ok(report)
    }

    // ---- index maintenance ----

    fn index_file_lines(&self, scope: &Scope) -> Result<Vec<String>, MemoryServiceError> {
        Ok(self
            .files
            .read_index(scope)?
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn write_index_lines(&self, scope: &Scope, lines: &[String]) -> Result<(), MemoryServiceError> {
        let mut text = lines.join("\n");
        if !text.is_empty() {
            text.push('\n');
        }
        self.files.write_index(scope, &text)?;
        Ok(())
    }

    fn append_index_line(
        &self,
        scope: &Scope,
        name: &str,
        description: &str,
    ) -> Result<(), MemoryServiceError> {
        let file = format!("{name}.md");
        let mut lines = self.index_file_lines(scope)?;
        lines.retain(|line| line_file(line).as_deref() != Some(file.as_str()));
        lines.push(format!("- [{name}]({file}) — {description}"));
        self.write_index_lines(scope, &lines)
    }

    fn remove_index_file(&self, scope: &Scope, file: &str) -> Result<(), MemoryServiceError> {
        let lines = self
            .index_file_lines(scope)?
            .into_iter()
            .filter(|line| line_file(line).as_deref() != Some(file))
            .collect::<Vec<_>>();
        self.write_index_lines(scope, &lines)
    }

    fn move_index_line(
        &self,
        from: &Scope,
        to: &Scope,
        name: &str,
        target_name: &str,
    ) -> Result<(), MemoryServiceError> {
        let old_file = format!("{name}.md");
        let mut source_lines = Vec::new();
        for line in self.index_file_lines(from)? {
            if line_file(&line).as_deref() == Some(old_file.as_str()) {
                let rewritten = if target_name == name {
                    line
                } else {
                    line.replace(&format!("]({old_file})"), &format!("]({target_name}.md)"))
                };
                source_lines.push(rewritten);
            }
        }
        let kept = self
            .index_file_lines(from)?
            .into_iter()
            .filter(|line| line_file(line).as_deref() != Some(old_file.as_str()))
            .collect::<Vec<_>>();
        self.write_index_lines(from, &kept)?;
        let mut target_lines = self.index_file_lines(to)?;
        let target_file = format!("{target_name}.md");
        target_lines.retain(|line| line_file(line).as_deref() != Some(target_file.as_str()));
        target_lines.extend(source_lines);
        self.write_index_lines(to, &target_lines)
    }
}

/// The file field of one index line, or `None` for a line that is not in the form.
fn line_file(line: &str) -> Option<String> {
    match radian_memory::index_lines(line).into_iter().next()? {
        radian_memory::IndexLine::Entry { file, .. } => Some(file),
        radian_memory::IndexLine::Invalid { .. } => None,
    }
}

/// The first free `<base>-N` name, `N` starting at 2.
fn unique_name(base: &str, taken: &mut std::collections::BTreeSet<String>) -> String {
    let mut n = 2u64;
    loop {
        let suffix = format!("-{n}");
        let max = radian_memory::NAME_MAX.saturating_sub(suffix.chars().count());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-memory-svc", label)
    }

    fn service(label: &str) -> (crate::test_support::TempDir, PathBuf, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let root = dir.join("memory");
        let service = MemoryService::open(&ws, &root);
        (dir, ws, service)
    }

    fn git_service(label: &str) -> (crate::test_support::TempDir, PathBuf, MemoryService) {
        let dir = scratch(label);
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        let root = dir.join("memory");
        let service = MemoryService::open(&ws, &root);
        (dir, ws, service)
    }

    #[test]
    fn a_write_registers_a_git_workspace_and_a_read_never_does() {
        let (dir, _ws, service) = git_service("auto-create");
        assert!(matches!(
            service.state().unwrap(),
            ProjectState::Unlinked { .. }
        ));
        assert!(service.list(ScopeSelection::Default).unwrap().is_empty());
        assert!(service.projects().unwrap().is_empty());

        let created = match service.project_for_write().unwrap() {
            ProjectForWrite::Created(created) => created,
            other => panic!("expected Created, got {other:?}"),
        };
        assert_eq!(created.identity_key, service.identity_key().to_string());
        assert!(created.candidates.is_empty(), "{created:?}");
        assert_eq!(service.projects().unwrap(), vec![created.project.clone()]);
        let notice = created.notice();
        assert!(notice.contains(&created.project.to_string()), "{notice}");
        assert!(notice.contains(&created.identity_key), "{notice}");

        let (scope, record) = service.add(WriteScope::Project, "note", "user").unwrap();
        assert_eq!(scope, Scope::Project(created.project.clone()));
        assert_eq!(record.frontmatter.source, "user");
        assert_eq!(
            service.project_for_write().unwrap(),
            ProjectForWrite::Linked(created.project)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_outside_a_repository_is_refused_and_creates_nothing() {
        let (dir, _ws, service) = service("not-a-workspace");
        let error = service.project_for_write().unwrap_err();
        let memory_not_saved = error.to_string();
        assert!(
            matches!(error, MemoryServiceError::NotAProjectWorkspace { .. }),
            "{memory_not_saved}"
        );
        assert!(
            memory_not_saved.contains("not a project workspace"),
            "{memory_not_saved}"
        );
        assert!(memory_not_saved.contains("not saved"), "{memory_not_saved}");
        assert!(
            !memory_not_saved.contains("global"),
            "the refusal must not suggest another scope: {memory_not_saved}"
        );
        assert!(
            !service.root().join("projects").exists(),
            "a refused write registered a project"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_still_creates_when_a_candidate_previous_project_exists() {
        let dir = scratch("moved-candidate");
        let root = dir.join("memory");
        let before = dir.join("here").join("radian");
        std::fs::create_dir_all(before.join(".git")).unwrap();
        let previous = MemoryService::open(&before, &root);
        let ProjectForWrite::Created(old_project) = previous.project_for_write().unwrap() else {
            panic!("the old workspace is a git workspace");
        };
        previous.add(WriteScope::Project, "old", "user").unwrap();

        let after_dir = dir.join("moved").join("radian");
        std::fs::create_dir_all(after_dir.join(".git")).unwrap();
        let after = MemoryService::open(&after_dir, &root);
        let ProjectForWrite::Created(created) = after.project_for_write().unwrap() else {
            panic!("a moved repository is created anew");
        };
        assert_eq!(created.candidates, vec![old_project.project.clone()]);
        assert_ne!(created.project, old_project.project);
        let notice = created.notice();
        assert!(notice.contains(&created.project.to_string()), "{notice}");
        assert!(
            notice.contains(&old_project.project.to_string()),
            "{notice}"
        );
        assert!(notice.contains("relink"), "{notice}");
        assert!(notice.contains("merge"), "{notice}");
        assert_eq!(after.projects().unwrap().len(), 2);
        assert!(
            after.list(ScopeSelection::ProjectOnly).unwrap().is_empty(),
            "nothing moved behind the user's back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unlinked_workspace_refuses_a_project_write_and_still_reads_global() {
        let (dir, _ws, service) = service("unlinked");
        assert!(matches!(
            service.state().unwrap(),
            ProjectState::Unlinked { .. }
        ));
        service.add(WriteScope::Global, "g", "user").unwrap();
        let global = service.list(ScopeSelection::Default).unwrap();
        assert_eq!(global.len(), 1);
        assert!(global[0].scope().is_global());

        let error = service.add(WriteScope::Project, "p", "user").unwrap_err();
        assert!(
            matches!(error, MemoryServiceError::Unlinked { .. }),
            "{error}"
        );
        assert!(!service.root().join("projects").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_project_write_reads_back_after_creating_the_project() {
        let (dir, _ws, service) = service("create");
        let project = service.create_project().unwrap();
        let (scope, record) = service.add(WriteScope::Project, "note", "user").unwrap();
        assert_eq!(scope, Scope::Project(project));
        assert_eq!(record.frontmatter.source, "user");
        assert_eq!(service.list(ScopeSelection::ProjectOnly).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            .add(WriteScope::Project, "shared", "user")
            .unwrap();

        let wt_service = MemoryService::open(&wt, &root);
        assert_eq!(wt_service.project_id().unwrap(), project);
        assert_eq!(
            wt_service.list(ScopeSelection::ProjectOnly).unwrap().len(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relinking_after_a_move_finds_the_original_records() {
        let dir = scratch("relink");
        let old_ws = dir.join("here").join("radian");
        std::fs::create_dir_all(&old_ws).unwrap();
        let root = dir.join("memory");
        let before = MemoryService::open(&old_ws, &root);
        let project = before.create_project().unwrap();
        before.add(WriteScope::Project, "kept", "user").unwrap();

        let new_ws = dir.join("moved").join("radian");
        std::fs::create_dir_all(&new_ws).unwrap();
        let after = MemoryService::open(&new_ws, &root);
        let state = after.state().unwrap();
        let ProjectState::Unlinked { candidates, .. } = &state else {
            panic!("expected Unlinked, got {state:?}");
        };
        assert_eq!(candidates, &vec![project.clone()]);
        assert!(matches!(
            after.add(WriteScope::Project, "x", "user"),
            Err(MemoryServiceError::Unlinked { .. })
        ));
        after.relink(&project).unwrap();
        assert_eq!(after.project_id().unwrap(), project);
        let entries = after.list(ScopeSelection::ProjectOnly).unwrap();
        assert_eq!(entries.len(), 1);
        let ScopedEntry::Record { frontmatter, .. } = &entries[0] else {
            panic!("expected a record");
        };
        assert_eq!(frontmatter.name, "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_then_list_shows_the_skeleton_and_the_index_line() {
        let (dir, _ws, service) = service("add-list");
        service.create_project().unwrap();
        service
            .add(WriteScope::Project, "note", "feedback")
            .unwrap();
        let entries = service.list(ScopeSelection::ProjectOnly).unwrap();
        assert_eq!(entries.len(), 1);
        let ScopedEntry::Record { frontmatter, .. } = &entries[0] else {
            panic!("expected a record");
        };
        assert_eq!(frontmatter.name, "note");
        assert_eq!(frontmatter.record_type, "feedback");
        assert_eq!(frontmatter.source, "user");
        let index = service
            .files()
            .read_index(&Scope::Project(service.project_id().unwrap()))
            .unwrap();
        assert!(index.contains("note.md"), "{index}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forget_deletes_the_file_and_its_index_line() {
        let (dir, _ws, service) = service("forget");
        service.create_project().unwrap();
        let (scope, _) = service.add(WriteScope::Project, "note", "user").unwrap();
        service.remove(&scope, "note").unwrap();
        assert!(
            service
                .list(ScopeSelection::ProjectOnly)
                .unwrap()
                .is_empty()
        );
        let index = service.files().read_index(&scope).unwrap();
        assert!(!index.contains("note.md"), "{index}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_renames_a_collision_instead_of_overwriting() {
        use radian_memory::NewRecord;
        let dir = scratch("merge-collision");
        let root = dir.join("memory");
        let service = MemoryService::open(&dir.join("ws"), &root);
        let project_a = service.registry().register("/work/a").unwrap();
        let project_b = service.registry().register("/work/b").unwrap();
        for project in [&project_a, &project_b] {
            service
                .files()
                .write(
                    &Scope::Project(project.clone()),
                    NewRecord {
                        name: "note".to_owned(),
                        description: String::new(),
                        record_type: "user".to_owned(),
                        source: "user".to_owned(),
                        provenance: None,
                        body: String::new(),
                    },
                )
                .unwrap();
        }

        let report = service.merge(&project_a, &project_b).unwrap();
        assert_eq!(report.conflicts, vec!["note"]);
        assert_eq!(
            report.renamed,
            vec![("note".to_owned(), "note-2".to_owned())]
        );
        assert_eq!(report.moved, vec!["note-2"]);
        assert_eq!(
            service
                .files()
                .list(&Scope::Project(project_a.clone()))
                .unwrap()
                .len(),
            2,
            "both files are in the merged project"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
