//! Session-owned retention of a **foreground** `shell` command's output.
//!
//! A foreground command reads through a bounded in-memory head+tail ([`crate::shell`]), so the
//! bytes between the head and the tail are normally gone. When such a result is truncated, the
//! model still needs them. This module tees the command's whole output to a private file in the
//! session's own directory (the jobs directory ADR 0039 decision 1 already established), and
//! hands the model an **opaque handle** — never a path.
//!
//! The registry is per [`crate::provider::CodingSession`] (its state, and the handle's nonce,
//! are fresh per session), so a handle from another session is unknown. A handle is
//! `out-<nonce>-<n>`; the nonce keeps two sessions in one process from ever colliding.
//!
//! The private root is held as an `O_DIRECTORY|O_NOFOLLOW` fd ([`crate::output::PrivateDir`]) and
//! every file is created/read relative to it (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600), so a symlink
//! planted at the root or swapped in later cannot redirect the output. The root is only enabled
//! when the provider has a **caller-supplied** session-private directory; a provider built
//! without one (`CodingTools::with_sandbox` and no `with_jobs`) has no retention and no
//! `shell_output` — there is no safe place to put it inside the model-readable workspace.
//!
//! Retention is bounded: at most [`Limits::max_foreground_retained_bytes`] per item and
//! [`Limits::max_foreground_session_bytes`] across the session (plus
//! [`Limits::max_foreground_items`] items). Making room evicts the oldest item, whose handle
//! then reads as unknown. A command whose output fits the model-facing cap creates no item and
//! leaves no handle: its file is removed as soon as the call ends (including an error path, via
//! [`RetentionInner`]'s `Drop`).

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use agent_core::{ToolConcurrency, ToolFailure, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::args;
use crate::fail;
use crate::output::PrivateDir;
use crate::provider::Limits;

/// One retained item, as the registry tracks it.
struct Item {
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
    /// private root (missing/unusable/symlinked); retention is then simply unavailable.
    root: OnceLock<Result<PrivateDir, String>>,
    /// A per-registry random token embedded in every handle, so two sessions in one process
    /// cannot resolve each other's handles even though they share the directory.
    nonce: u64,
    next: AtomicU64,
    max_item: u64,
    max_session: u64,
    max_items: usize,
    state: Mutex<State>,
}

impl ForegroundOutputs {
    pub(crate) fn new(dir: PathBuf, limits: &Limits) -> Self {
        Self {
            inner: Arc::new(ForegroundInner {
                dir,
                root: OnceLock::new(),
                nonce: random_nonce(),
                next: AtomicU64::new(1),
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

    /// Creates the retained file for one command and returns its sink. `None` when there is no
    /// usable private root, or the file cannot be created exclusively: the shell still runs, it
    /// just has no retrievable handle.
    pub(crate) fn open(&self) -> Option<Retention> {
        let root = self.root()?;
        let n = self.inner.next.fetch_add(1, Ordering::SeqCst);
        let handle = format!("out-{:016x}-{n}", self.inner.nonce);
        let name = format!("{handle}.out");
        let file = root.create(std::ffi::OsStr::new(&name)).ok()?;
        Some(Retention {
            inner: Arc::new(RetentionInner {
                handle,
                path: self.inner.dir.join(name),
                cap: self.inner.max_item,
                file: Mutex::new(file),
                retained: AtomicU64::new(0),
                total: AtomicU64::new(0),
                finished: AtomicBool::new(false),
            }),
        })
    }

    /// Commits a finished command's retention. `keep` inserts the item (evicting the oldest to
    /// stay inside the session bounds) and returns its handle; otherwise the file is removed and
    /// no handle exists.
    pub(crate) fn finish(&self, retention: &Retention, keep: bool) -> Option<String> {
        // Stop the `Drop` guard first: it must not remove a file this method is about to keep,
        // and it must not race the explicit removal below.
        retention.inner.finished.store(true, Ordering::SeqCst);
        let handle = retention.handle().to_owned();
        let path = retention.path().to_owned();
        let retained = retention.retained();
        if !keep {
            {
                let mut state = lock(&self.inner.state);
                state.items.remove(&handle);
                state.order.retain(|kept| kept != &handle);
            }
            let _ = std::fs::remove_file(&path);
            return None;
        }

        let mut evicted: Vec<PathBuf> = Vec::new();
        {
            let mut state = lock(&self.inner.state);
            state.order.push_back(handle.clone());
            state.items.insert(handle.clone(), Item { path, retained });
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
                    evicted.push(item.path);
                }
            }
        }
        for path in evicted {
            let _ = std::fs::remove_file(path);
        }
        Some(handle)
    }

    /// Opens a kept item for reading. An unknown, malformed or cross-session handle is
    /// `unknown_output` — never a filesystem lookup.
    pub(crate) fn open_read(&self, handle: &str) -> Result<File, ToolFailure> {
        if !lock(&self.inner.state).items.contains_key(handle) {
            return Err(unknown_output(handle));
        }
        let root = self.root().ok_or_else(|| unknown_output(handle))?;
        root.open_read(std::ffi::OsStr::new(&format!("{handle}.out")))
            .map_err(|error| crate::output::read_failure("output_read", &error))
    }

    /// Session end: drop every kept item and release its file. Synchronous file removal only, so
    /// the normal shutdown path leaves no retained output behind. The panic path never calls
    /// this; the state drops with the session instead.
    pub(crate) fn shutdown(&self) {
        let paths: Vec<PathBuf> = {
            let mut state = lock(&self.inner.state);
            state.order.clear();
            state.total = 0;
            state
                .items
                .drain()
                .map(|(_, item)| item.path)
                .collect::<Vec<_>>()
        };
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct RetentionInner {
    handle: String,
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

    pub(crate) fn handle(&self) -> &str {
        &self.inner.handle
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

/// A process-wide unique token from the standard library's random hasher state (no dependency).
fn random_nonce() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    RandomState::new().build_hasher().finish()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------- the `shell_output` tool ----------

/// The read arguments: the opaque handle plus the shared file reads.
#[derive(Deserialize)]
struct ReadArgs {
    handle: String,
    #[serde(flatten)]
    file: crate::output::FileArgs,
}

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

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "shell_output".into(),
        description: "Read the output a **foreground** `shell` command produced, when its \
                      result said bytes were discarded. Pass the opaque `output_handle` from \
                      that result; the output file is outside the workspace and this is the \
                      only way to reach it (a path is refused). Read the last N lines with \
                      `tail_lines`, a line range with `from_line`/`max_lines` (numbered, so \
                      you can page), or only matching lines with `pattern` (a Rust regex). \
                      The retained output is itself bounded; the `shell` result says when it \
                      was cut. A handle belongs to the session that produced it."
            .into(),
        input_schema: schema(),
        concurrency: ToolConcurrency::Shared,
    }
}

/// Reads a retained foreground output. The handle resolves only inside this registry; anything
/// else is `unknown_output`, never a path open.
pub(crate) fn run_read(
    foreground: &ForegroundOutputs,
    limits: &Limits,
    arguments: &Value,
) -> Result<ToolOutput, ToolFailure> {
    let args: ReadArgs = args::parse("shell_output", &schema(), arguments)?;
    let file = foreground.open_read(&args.handle)?;
    let mode = crate::output::FileMode::from_args("shell_output", &args.file, arguments)?
        .ok_or_else(|| {
            args::invalid(
                "shell_output",
                arguments,
                ["give `tail_lines`, `from_line`/`max_lines` or `pattern`"],
            )
        })?;
    crate::output::read_file(
        &file,
        "handle",
        &args.handle,
        mode,
        limits.default_read_bytes,
        limits.default_grep_matches,
        "output_read",
    )
}

fn unknown_output(handle: &str) -> ToolFailure {
    fail(
        "unknown_output",
        format!(
            "no retained output with handle `{handle}` in this session; the handle is the one \
             a truncated `shell` result returned, and it is not a path"
        ),
    )
}
