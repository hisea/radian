//! The project registry (ADR 0021 D2): an append-only `projects.jsonl` mapping an identity
//! key (a git common dir path, or a canonical workspace path outside git) to a stable
//! `ProjectId`. Relinking attaches a key to an existing project; a key that is not found is
//! never silently given a new identity.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::MemoryError;
use crate::log::Log;
use crate::model::{ProjectId, now_utc};

/// The two registry events.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistryEvent {
    Registered {
        identity_key: String,
        project_id: ProjectId,
        at: String,
    },
    Relinked {
        identity_key: String,
        project_id: ProjectId,
        at: String,
    },
}

impl RegistryEvent {
    const NAMES: &'static [&'static str] = &["ProjectRegistered", "ProjectRelinked"];

    fn name(&self) -> &'static str {
        match self {
            RegistryEvent::Registered { .. } => "ProjectRegistered",
            RegistryEvent::Relinked { .. } => "ProjectRelinked",
        }
    }

    fn to_data(&self) -> Value {
        match self {
            RegistryEvent::Registered {
                identity_key,
                project_id,
                at,
            }
            | RegistryEvent::Relinked {
                identity_key,
                project_id,
                at,
            } => serde_json::json!({
                "identity_key": identity_key,
                "project_id": project_id,
                "at": at,
            }),
        }
    }

    fn from_data(name: &str, data: &Value) -> Result<Self, serde_json::Error> {
        #[derive(serde::Deserialize)]
        struct Data {
            identity_key: String,
            project_id: ProjectId,
            #[allow(dead_code)]
            #[serde(default)]
            at: String,
        }
        let data: Data = serde_json::from_value(data.clone())?;
        Ok(match name {
            "ProjectRegistered" => RegistryEvent::Registered {
                identity_key: data.identity_key,
                project_id: data.project_id,
                at: data.at,
            },
            "ProjectRelinked" => RegistryEvent::Relinked {
                identity_key: data.identity_key,
                project_id: data.project_id,
                at: data.at,
            },
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown registry event {other:?}"
                )));
            }
        })
    }

    fn apply(&self, by_key: &mut BTreeMap<String, ProjectId>, projects: &mut BTreeSet<ProjectId>) {
        match self {
            // First registration wins; a later `register` for the same key returns the
            // existing id instead of appending another line.
            RegistryEvent::Registered {
                identity_key,
                project_id,
                ..
            } => {
                by_key
                    .entry(identity_key.clone())
                    .or_insert(project_id.clone());
                projects.insert(project_id.clone());
            }
            RegistryEvent::Relinked {
                identity_key,
                project_id,
                ..
            } => {
                by_key.insert(identity_key.clone(), project_id.clone());
                projects.insert(project_id.clone());
            }
        }
    }
}

#[derive(Default)]
struct State {
    by_key: BTreeMap<String, ProjectId>,
    projects: BTreeSet<ProjectId>,
}

/// Opens (creating if needed) the registry log and replays it.
fn open_and_replay(path: &Path) -> Result<(Log, State), MemoryError> {
    if let Some(parent) = path.parent() {
        crate::fs::ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
    }
    let (log, events) = Log::open(path, RegistryEvent::NAMES)?;
    let mut state = State::default();
    for event in &events {
        let parsed = RegistryEvent::from_data(&event.event, &event.data).map_err(|e| {
            MemoryError::Corrupt {
                path: path.to_path_buf(),
                line: event.line,
                column: 0,
                message: e.to_string(),
            }
        })?;
        parsed.apply(&mut state.by_key, &mut state.projects);
    }
    Ok((log, state))
}

/// The project registry rooted at `<root>/projects.jsonl`.
#[derive(Debug, Clone)]
pub struct ProjectRegistry {
    root: PathBuf,
}

impl ProjectRegistry {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("projects.jsonl")
    }

    /// The project id an identity key currently maps to.
    pub fn resolve(&self, identity_key: &str) -> Result<Option<ProjectId>, MemoryError> {
        let (_, state) = open_and_replay(&self.path())?;
        Ok(state.by_key.get(identity_key).cloned())
    }

    /// The id for `identity_key`, creating a new project only if the key is unknown. The
    /// same key always returns the same id.
    pub fn register(&self, identity_key: &str) -> Result<ProjectId, MemoryError> {
        let path = self.path();
        let (mut log, state) = open_and_replay(&path)?;
        if let Some(existing) = state.by_key.get(identity_key) {
            return Ok(existing.clone());
        }
        let project_id = ProjectId::generate();
        let event = RegistryEvent::Registered {
            identity_key: identity_key.to_owned(),
            project_id: project_id.clone(),
            at: now_utc(),
        };
        log.append(event.name(), true, &event.to_data())?;
        Ok(project_id)
    }

    /// Attaches `identity_key` to an existing `project_id` (the user chose it).
    pub fn relink(&self, identity_key: &str, project_id: &ProjectId) -> Result<(), MemoryError> {
        let path = self.path();
        let (mut log, state) = open_and_replay(&path)?;
        if !state.projects.contains(project_id) {
            return Err(MemoryError::UnknownProject(project_id.clone()));
        }
        let event = RegistryEvent::Relinked {
            identity_key: identity_key.to_owned(),
            project_id: project_id.clone(),
            at: now_utc(),
        };
        log.append(event.name(), true, &event.to_data())
    }

    /// Every project id the registry knows.
    pub fn projects(&self) -> Result<Vec<ProjectId>, MemoryError> {
        let (_, state) = open_and_replay(&self.path())?;
        Ok(state.projects.into_iter().collect())
    }

    /// Every identity key attached to a project.
    pub fn identity_keys(&self, project_id: &ProjectId) -> Result<Vec<String>, MemoryError> {
        let (_, state) = open_and_replay(&self.path())?;
        Ok(state
            .by_key
            .into_iter()
            .filter_map(|(key, id)| (id == *project_id).then_some(key))
            .collect())
    }
}
