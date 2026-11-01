//! The project registry (ADR 0021 D2): an append-only `projects.jsonl` mapping an identity
//! key (a git common dir path, or a canonical workspace path outside git) to a stable
//! `ProjectId`. Relinking attaches a key to an existing project; a key that is not found is
//! never silently given a new identity.
//!
//! The registry keeps its own small append-only log: a single-writer `flock`, one fsync per
//! append, a torn tail dropped before the next append, and a malformed line named by its
//! 1-based line. The old memory store's event log is gone; this is the one JSONL file the
//! file store still writes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::MemoryError;
use crate::fs::ensure_private_dir;
use crate::model::{ProjectId, now_utc};

const SCHEMA_VERSION: u64 = 1;

/// One accepted registry line: its 1-based line, event name and `data` object.
type Events = Vec<(u64, String, Value)>;

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

/// One open registry log: the file (holding the writer lock), its path and the next `seq`.
struct Log {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

impl Log {
    /// Opens (creating if needed) the log, takes the writer lock and returns the accepted
    /// events in file order.
    fn open(path: &Path) -> Result<(Self, Events), MemoryError> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(crate::fs::PRIVATE_FILE_MODE);
        }
        let mut file = options.open(path).map_err(|e| MemoryError::io(path, e))?;
        file.lock_exclusive().map_err(|_| MemoryError::Locked {
            path: path.to_path_buf(),
        })?;
        let (events, committed, next_seq) = scan(&mut file, path, RegistryEvent::NAMES)?;
        let len = file.metadata().map_err(|e| MemoryError::io(path, e))?.len();
        if len > committed {
            file.set_len(committed)
                .map_err(|e| MemoryError::io(path, e))?;
            file.seek(SeekFrom::End(0))
                .map_err(|e| MemoryError::io(path, e))?;
        }
        Ok((
            Self {
                path: path.to_path_buf(),
                file,
                next_seq,
            },
            events,
        ))
    }

    fn append(&mut self, name: &str, critical: bool, data: &Value) -> Result<(), MemoryError> {
        let line = Line {
            schema_version: SCHEMA_VERSION,
            seq: self.next_seq,
            ts: now_utc(),
            event: name,
            critical,
            data,
        };
        let mut bytes = serde_json::to_vec(&line)
            .map_err(|e| MemoryError::io(&self.path, std::io::Error::other(e)))?;
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|e| MemoryError::io(&self.path, e))?;
        self.file
            .flush()
            .map_err(|e| MemoryError::io(&self.path, e))?;
        self.file
            .sync_all()
            .map_err(|e| MemoryError::io(&self.path, e))?;
        self.next_seq += 1;
        Ok(())
    }
}

/// Streams the log, applying the §3.2/§7 rules. Returns the accepted events, the committed
/// byte length (everything through the last LF) and the next free `seq`.
fn scan(file: &mut File, path: &Path, known: &[&str]) -> Result<(Events, u64, u64), MemoryError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|e| MemoryError::io(path, e))?;
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut lineno: u64 = 0;
    let mut expect_seq: u64 = 0;
    let mut committed: u64 = 0;
    let mut schema: Option<u64> = None;
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| MemoryError::io(path, e))?;
        if n == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            break;
        }
        lineno += 1;
        let raw =
            std::str::from_utf8(&line[..line.len() - 1]).map_err(|e| MemoryError::Corrupt {
                path: path.to_path_buf(),
                line: lineno,
                column: e.valid_up_to(),
                message: "line is not valid UTF-8".into(),
            })?;
        let envelope: Envelope = serde_json::from_str(raw).map_err(|e| MemoryError::Corrupt {
            path: path.to_path_buf(),
            line: lineno,
            column: e.column(),
            message: e.to_string(),
        })?;
        match schema {
            None => {
                if envelope.schema_version != SCHEMA_VERSION {
                    return Err(MemoryError::Envelope {
                        path: path.to_path_buf(),
                        line: lineno,
                        message: format!(
                            "unsupported schema_version {} (expected {SCHEMA_VERSION})",
                            envelope.schema_version
                        ),
                    });
                }
                schema = Some(envelope.schema_version);
            }
            Some(version) => {
                if envelope.schema_version != version {
                    return Err(MemoryError::Envelope {
                        path: path.to_path_buf(),
                        line: lineno,
                        message: "schema_version changed mid-log".into(),
                    });
                }
            }
        }
        if envelope.seq != expect_seq {
            return Err(MemoryError::BadSeq {
                path: path.to_path_buf(),
                line: lineno,
                expected: expect_seq,
                found: envelope.seq,
            });
        }
        expect_seq += 1;
        committed += n as u64;
        if !known.contains(&envelope.event.as_str()) {
            if envelope.critical {
                return Err(MemoryError::UnknownCritical {
                    path: path.to_path_buf(),
                    line: lineno,
                    event: envelope.event,
                });
            }
            continue;
        }
        events.push((lineno, envelope.event, envelope.data));
    }
    // `expect_seq` counts every committed line, including skipped unknown events, so it is
    // the next free seq.
    Ok((events, committed, expect_seq))
}

/// Opens (creating if needed) the registry log and replays it.
fn open_and_replay(path: &Path) -> Result<(Log, State), MemoryError> {
    let (log, events) = Log::open(path)?;
    let mut state = State::default();
    for (line, event, data) in &events {
        let parsed = RegistryEvent::from_data(event, data).map_err(|e| MemoryError::Corrupt {
            path: path.to_path_buf(),
            line: *line,
            column: 0,
            message: e.to_string(),
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

#[derive(Serialize)]
struct Line<'a> {
    schema_version: u64,
    seq: u64,
    ts: String,
    event: &'a str,
    critical: bool,
    data: &'a Value,
}

#[derive(Deserialize)]
struct Envelope {
    schema_version: u64,
    seq: u64,
    #[allow(dead_code)]
    ts: String,
    event: String,
    critical: bool,
    data: Value,
}
