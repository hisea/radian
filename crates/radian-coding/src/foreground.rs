//! Session-owned retention of a **foreground** `shell` command's output.
//!
//! A foreground command reads through a bounded in-memory head+tail ([`crate::shell`]), so the
//! bytes between the head and the tail are normally gone. When such a result is truncated, the
//! model still needs them. This module tees the command's whole output to a private file in the
//! session's own directory (the jobs directory ADR 0039 decision 1 already established), and
//! registers it in the **same job registry** as a settled entry with a normal `job-…` id (ADR
//! 0037 decision 3a): one id space, one reader (`job`), no journal records for it.
//!
//! The registry is per [`crate::provider::CodingSession`]. Retention is bounded: at most
//! [`Limits::max_foreground_retained_bytes`] per item and [`Limits::max_foreground_session_bytes`]
//! across the session (plus [`Limits::max_foreground_items`] items). Making room evicts the
//! oldest retained entry, whose id then reads as unknown.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use agent_core::{ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args;
use crate::jobs::{JobState, Jobs, Ring};
use crate::output::PrivateDir;
use crate::provider::Limits;

/// One retained item, as the registry tracks it.
struct Item {
    /// The job id the model reads it with (ADR 0037 decision 3a: one id space).
    id: String,
    path: PathBuf,
    retained: u64,
}

#[derive(Default)]
struct State {
    /// Handles in keep order, oldest first (eviction is from the front).
    order: VecDeque<String>,
    items: HashMap<String, Item>,
    /// The sum of every kept item's retained bytes.
    total: u64,
}

/// The session's retained-output registry. Cheap to clone the `Arc`, never the map.
pub(crate) struct ForegroundOutputs {
    inner: Arc<ForegroundInner>,
}

struct ForegroundInner {
    /// The private directory the files live in. A caller-supplied, session-owned directory
    /// outside the workspace (ADR 0039 decision 1), never a workspace path.
    dir: PathBuf,
    /// The root fd, opened lazily on the first retained command. `Err` means there is no safe
    /// private root (missing/unusable/symlinked); retention is then unavailable.
    root: OnceLock<Result<PrivateDir, String>>,
    /// The job registry the retained entries are registered in (ADR 0037 decision 3a).
    jobs: Arc<Jobs>,
    max_item: u64,
    max_session: u64,
    max_items: usize,
    state: Mutex<State>,
}

impl ForegroundOutputs {
    pub(crate) fn new(dir: PathBuf, limits: &Limits, jobs: &Arc<Jobs>) -> Self {
        Self {
            inner: Arc::new(ForegroundInner {
                dir,
                root: OnceLock::new(),
                jobs: Arc::clone(jobs),
                max_item: limits.max_foreground_retained_bytes as u64,
                max_session: limits.max_foreground_session_bytes as u64,
                max_items: limits.max_foreground_items,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// The private root, opened once. `None` when it cannot be used safely.
    fn root(&self) -> Option<&PrivateDir> {
        self.inner
            .root
            .get_or_init(|| PrivateDir::open(&self.inner.dir).map_err(|e| e.to_string()))
            .as_ref()
            .ok()
    }

    /// Creates the retained file for one command and returns its sink. The file is named for
    /// the job id the model will read it with (ADR 0037 decision 3a: one id space, minted
    /// from the registry's counter). `None` when there is no usable private root, or the file
    /// cannot be created exclusively: the shell still runs, it just has no retrievable id.
    pub(crate) fn open(&self, command: &str) -> Option<Retention> {
        let root = self.root()?;
        let id = crate::jobs::next_job_id();
        let name = format!("{id}.out");
        let file = root.create(std::ffi::OsStr::new(&name)).ok()?;
        Some(Retention {
            inner: Arc::new(RetentionInner {
                id,
                command: command.to_owned(),
                path: self.inner.dir.join(name),
                cap: self.inner.max_item,
                file: Mutex::new(file),
                retained: AtomicU64::new(0),
                total: AtomicU64::new(0),
                finished: AtomicBool::new(false),
            }),
        })
    }

    /// Commits a finished command's retention. `keep` registers the item in the job registry
    /// as a settled entry (evicting the oldest retained entries to stay inside the session
    /// bounds) and returns its job id; otherwise the file is removed and no id exists.
    /// An evicted or shut-down entry's id reads as unknown.
    pub(crate) fn finish(&self, retention: &Retention, keep: Option<JobState>) -> Option<String> {
        // Stop the `Drop` guard first: it must not remove a file this method is about to keep,
        // and it must not race the explicit removal below.
        retention.inner.finished.store(true, Ordering::SeqCst);
        let id = retention.id().to_owned();
        let command = retention.inner.command.clone();
        let path = retention.path().to_owned();
        let retained = retention.retained();
        let total = retention.total();
        if keep.is_none() {
            {
                let mut state = lock(&self.inner.state);
                state.items.remove(&id);
                state.order.retain(|kept| kept != &id);
            }
            let _ = std::fs::remove_file(&path);
            return None;
        }
        // The entry's increment starts at byte 0, bounded by the ring (ADR 0037 decision 4).
        let ring = Ring::seeded(
            tail_of(&path, (retained as usize).min(crate::jobs::JOB_RING_BYTES)),
            total,
        );
        self.inner.jobs.insert_settled(
            id.clone(),
            command,
            keep.expect("checked above"),
            path.clone(),
            ring,
        );

        let mut evicted: Vec<(String, PathBuf)> = Vec::new();
        {
            let mut state = lock(&self.inner.state);
            state.order.push_back(id.clone());
            state.items.insert(
                id.clone(),
                Item {
                    id: id.clone(),
                    path,
                    retained,
                },
            );
            state.total += retained;
            // Keep at least the item just committed, even if it alone exceeds the session cap;
            // its own per-item cap is the hard bound.
            while (state.total > self.inner.max_session || state.items.len() > self.inner.max_items)
                && state.order.len() > 1
            {
                let Some(oldest) = state.order.pop_front() else {
                    break;
                };
                if let Some(item) = state.items.remove(&oldest) {
                    state.total = state.total.saturating_sub(item.retained);
                    evicted.push((item.id, item.path));
                }
            }
        }
        for (id, path) in evicted {
            self.inner.jobs.remove(&id);
            let _ = std::fs::remove_file(path);
        }
        Some(id)
    }

    /// Session end: drop every kept item, unregister its id and remove its file. Synchronous
    /// file removal only, so the normal shutdown path leaves no retained output behind. The
    /// panic path never calls this; the state drops with the session instead.
    pub(crate) fn shutdown(&self) {
        let ids: Vec<(String, PathBuf)> = {
            let mut state = lock(&self.inner.state);
            state.order.clear();
            state.total = 0;
            state
                .items
                .drain()
                .map(|(_, item)| (item.id, item.path))
                .collect::<Vec<_>>()
        };
        for (id, path) in ids {
            self.inner.jobs.remove(&id);
            let _ = std::fs::remove_file(path);
        }
    }
}

struct RetentionInner {
    /// The job id the model reads the entry with (ADR 0037 decision 3a: one id space).
    id: String,
    /// The command's first line, for the registry entry.
    command: String,
    path: PathBuf,
    cap: u64,
    file: Mutex<File>,
    /// Bytes actually in the file, capped at `cap`.
    retained: AtomicU64,
    /// Every byte the command produced, kept or not.
    total: AtomicU64,
    /// Set by [`ForegroundOutputs::finish`]: the item is committed, so `Drop` must not touch
    /// the file again.
    finished: AtomicBool,
}

/// One command's live retention sink. Cloned into each stream reader; the `Arc` is shared, so a
/// chunk from either stream appends to the same file, capped and counted.
#[derive(Clone)]
pub(crate) struct Retention {
    inner: Arc<RetentionInner>,
}

impl Retention {
    /// Appends `bytes` to the retained file while there is room, always counting the total. The
    /// file lock serializes the two stream readers, so their lines stay whole.
    pub(crate) fn push(&self, bytes: &[u8]) {
        self.inner
            .total
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let mut file = lock(&self.inner.file);
        let retained = self.inner.retained.load(Ordering::Relaxed);
        if retained >= self.inner.cap {
            return;
        }
        let take = (bytes.len() as u64).min(self.inner.cap - retained) as usize;
        if take > 0 {
            let _ = file.write_all(&bytes[..take]);
            self.inner
                .retained
                .fetch_add(take as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.inner.id
    }

    fn path(&self) -> &std::path::Path {
        &self.inner.path
    }

    pub(crate) fn retained(&self) -> u64 {
        self.inner.retained.load(Ordering::Relaxed)
    }

    pub(crate) fn total(&self) -> u64 {
        self.inner.total.load(Ordering::Relaxed)
    }

    /// Whether the per-item cap cut the file (some produced bytes are not retained).
    pub(crate) fn is_cut(&self) -> bool {
        self.total() > self.retained()
    }
}

/// The cleanup guard for an error or cancellation path: if no [`ForegroundOutputs::finish`]
/// committed the item, the last clone drops and removes the private file. No asynchronous work,
/// and nothing on a panic unwind (the file stays in the private directory).
impl Drop for RetentionInner {
    fn drop(&mut self) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        if std::thread::panicking() {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The last `max` bytes of a retained file, for seeding a settled entry's ring (ADR 0037
/// decision 4: the increment starts at byte 0, bounded by the ring). Empty when the file
/// cannot be read; the entry then states the whole total as dropped.
fn tail_of(path: &Path, max: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    let Ok(mut file) = crate::output::open_output_read(path) else {
        return bytes;
    };
    let Ok(end) = file.seek(SeekFrom::End(0)) else {
        return bytes;
    };
    if file
        .seek(SeekFrom::Start(end.saturating_sub(max as u64)))
        .is_err()
    {
        return Vec::new();
    }
    let _ = file.read_to_end(&mut bytes);
    bytes
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- the `shell_output` one-version alias (ADR 0043 decision 3) ----------

/// The read arguments: the old opaque handle (now a job id) plus the shared file reads.
/// A handle from before the rename never resolves; a current job id does.
#[derive(Deserialize)]
pub(crate) struct ReadArgs {
    pub(crate) handle: String,
    #[serde(flatten)]
    pub(crate) file: crate::output::FileArgs,
}

/// The argument shape the alias parses (the old `shell_output` schema).
pub(crate) fn schema() -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "handle".into(),
        json!({
            "type": "string",
            "description": "The opaque `output_handle` a truncated foreground `shell` result \
                            returned. It is not a path; a path or a handle from another \
                            session is refused."
        }),
    );
    if let Some(fields) = crate::output::file_properties().as_object() {
        for (key, value) in fields {
            properties.insert(key.clone(), value.clone());
        }
    }
    properties.insert(
        "mode".into(),
        json!({
            "type": "string",
            "enum": ["tail", "range", "pattern"],
            "description": "Which read this is: `tail` (`tail_lines`), `range` \
                            (`from_line`/`max_lines`) or `pattern` (`pattern`). With `mode`, \
                            the other reads' fields are ignored; without it, give at most one \
                            of them."
        }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": ["handle"],
        "additionalProperties": false
    })
}

pub(crate) fn alias_spec() -> ToolSpec {
    ToolSpec {
        name: "shell_output".into(),
        description: "Deprecated alias for `job` with `{\"op\": \"read\"}`; kept for one \
                      version."
            .into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

/// `shell_output`, the one-version alias for `job { op: read }` (ADR 0043 decision 3). The
/// old arguments read the same full-file modes; the `handle` is looked up in the job registry
/// (a current job id resolves, a pre-rename handle does not). The result opens with the
/// rename line.
pub(crate) fn run_alias(
    jobs: &Jobs,
    limits: &Limits,
    arguments: &Value,
) -> Result<ToolOutput, ToolFailure> {
    use crate::jobs::{SHELL_OUTPUT_RENAME, read_output_file, rename_failure, renamed};
    let args: ReadArgs = args::parse("shell_output", &schema(), arguments)?;
    let mode = crate::output::FileMode::from_args("shell_output", &args.file, arguments)?
        .ok_or_else(|| {
            args::invalid(
                "shell_output",
                arguments,
                ["give `tail_lines`, `from_line`/`max_lines` or `pattern`"],
            )
        })?;
    read_output_file(jobs, &args.handle, mode, limits)
        .map(|output| renamed(SHELL_OUTPUT_RENAME, output))
        .map_err(|failure| rename_failure(SHELL_OUTPUT_RENAME, failure))
}
