//! The write side: a real, fsyncing [`agent_core::TranscriptJournal`] on a JSONL file.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_core::{JournalError, JournalRecord, Message, ModelUsage, ToolResult, TranscriptJournal};
use fs2::FileExt;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::mpsc::UnboundedSender;

use crate::ReadError;
use crate::fee::UsageFee;
use crate::gate::SegmentGate;
use crate::runtime::{RuntimeEvent, runtime_data};

/// The schema version this crate writes and accepts.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// How long [`SessionJournal::open_locked`] keeps retrying a contended writer lock before it
/// reports the existing [`WriteError::Locked`]. Long enough to ride out the `[fork, exec)`
/// window of a child another thread is spawning; far shorter than any genuine second writer
/// holds the lock.
const LOCK_RETRY_BUDGET: Duration = Duration::from_millis(200);
/// The first retry delay; each retry doubles it (`1, 2, 4, …` ms) until the budget is spent.
const LOCK_RETRY_FIRST: Duration = Duration::from_millis(1);

/// The parent a derived session came from. For a fork (ADR 0022) it names the parent and the
/// fork point — the seq of a `TurnFinished` (or 0 for "the session start"), which is also what
/// the parent's `SessionForked` records.
///
/// `relation` distinguishes derived-session kinds. A `parent` written before the `relation` field carries no
/// `relation` and reads as `"fork"` via `default_relation`; ADR 0006 §7(a) forward
/// compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkParent {
    pub session_id: String,
    pub seq: u64,
    #[serde(default = "default_relation")]
    pub relation: String,
}

/// The `relation` of a `parent` line written before ADR 0043 added the field: an old line read
/// by a new reader is a fork (ADR 0006 §7a).
fn default_relation() -> String {
    "fork".to_owned()
}

impl ForkParent {
    /// A fork's parent (ADR 0022): the lineage at the parent's fork point.
    pub fn fork(session_id: impl Into<String>, seq: u64) -> Self {
        Self {
            session_id: session_id.into(),
            seq,
            relation: "fork".to_owned(),
        }
    }

    /// A one-shot subagent child's parent (ADR 0043 decision 1a): the parent session and the
    /// seq it was spawned at (`0` when the spawning call does not know a boundary).
    pub fn subagent(session_id: impl Into<String>, seq: u64) -> Self {
        Self {
            session_id: session_id.into(),
            seq,
            relation: "subagent".to_owned(),
        }
    }

    /// The relation. A `parent` without one reads back as `"fork"` (ADR 0006 §7a).
    pub fn relation(&self) -> &str {
        &self.relation
    }
}

/// What a new session file's `SessionCreated` line carries. `workspace` and
/// `config_snapshot` are required by ADR 0006 §5.1. `parent` is set only for a forked session
/// (ADR 0022); the other reserved optional fields (`agent_id`/`profile_ref`/
/// `workspace_binding`) stay unexposed and unvalidated.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCreatedData {
    pub workspace: String,
    pub config_snapshot: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<ForkParent>,
}

impl SessionCreatedData {
    pub fn new(workspace: impl Into<String>, config_snapshot: Value) -> Self {
        Self {
            workspace: workspace.into(),
            config_snapshot,
            parent: None,
        }
    }

    /// The `SessionCreated` payload of a forked child: `parent` names the parent and its fork
    /// point (ADR 0022 §3).
    pub fn forked(
        workspace: impl Into<String>,
        config_snapshot: Value,
        parent: ForkParent,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            config_snapshot,
            parent: Some(parent),
        }
    }
}

/// Why a session file could not be opened for writing.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("{path}: cannot open: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: already locked by another writer; a session file allows exactly one writer")]
    Locked { path: PathBuf },
    #[error(
        "{path}: the file already holds a complete session; refusing to overwrite it \
         (call `resume` instead)"
    )]
    NotEmpty { path: PathBuf },
    #[error("{path}: the file is empty; it was never created, call `create` first")]
    Empty { path: PathBuf },
    #[error("{path}: reading the existing file for resume failed: {source}")]
    Scan {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: write failed: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Policy(#[from] ReadError),
    #[error(
        "{path}: a transcript replacement write failed; the session needs recovery, so no          further events can be written"
    )]
    SegmentPoisoned { path: PathBuf },
}

/// Event names and their `critical` flags, exactly as ADR 0006 §4 pins them for the
/// records this journal writes.
const EVT_SESSION_CREATED: (&str, bool) = ("SessionCreated", true);
const EVT_MESSAGE_APPENDED: (&str, bool) = ("MessageAppended", true);
const EVT_TRANSCRIPT_REPLACED: (&str, bool) = ("TranscriptReplaced", true);
const EVT_TRANSCRIPT_REPLACEMENT_COMMITTED: (&str, bool) = ("TranscriptReplacementCommitted", true);
const EVT_TRANSCRIPT_REPLACEMENT_ABORTED: (&str, bool) = ("TranscriptReplacementAborted", true);
const EVT_TOOL_STARTED: (&str, bool) = ("ToolExecutionStarted", true);
const EVT_TOOL_SETTLED: (&str, bool) = ("ToolExecutionSettled", true);
const EVT_USAGE: (&str, bool) = ("ModelUsageRecorded", false);
const EVT_COMPACTION_USAGE: (&str, bool) = ("CompactionUsageRecorded", false);
const EVT_CONTEXT_COMPACTED: (&str, bool) = ("ContextCompacted", false);
/// The runtime-event names that are also sync points (ADR 0040 decision 2): the writer must
/// not depend on `radian-session`'s event type here, only on the name, so they are named
/// beside the core's own event constants.
const EVT_TURN_FINISHED: &str = "TurnFinished";
const EVT_TURN_FAILED: &str = "TurnFailed";

/// Whether an append of `event` is a durability boundary (ADR 0040 decision 2): every record
/// written before it in this writer is on disk when its append returns. Everything else is
/// `write`+`flush`ed and rides the next sync point. The names are exactly the v1 disk event
/// names; a record whose name is not here is not synced.
fn must_sync(event: &str) -> bool {
    [
        // The first line.
        EVT_SESSION_CREATED.0,
        // Before a tool runs (and before a background `shell` start, which is a tool call). It
        // also flushes the assistant message that declared the calls.
        EVT_TOOL_STARTED.0,
        // Turn boundaries.
        EVT_TURN_FINISHED,
        EVT_TURN_FAILED,
        // The replacement commit (ADR 0019) and the abort its `resume` writes.
        EVT_TRANSCRIPT_REPLACEMENT_COMMITTED.0,
        EVT_TRANSCRIPT_REPLACEMENT_ABORTED.0,
    ]
    .contains(&event)
}

/// Computes the fee for one usage record (ADR 0027). The host installs it on the
/// journal; the writer resolves the route in force and prices the call, or returns `None` when
/// it cannot (no price: the record carries no fee, never an invented number).
pub type UsageFeeFn = Arc<dyn Fn(&ModelUsage) -> Option<UsageFee> + Send + Sync>;

/// One JSONL session file plus its single-writer lock and running `seq` counter. It
/// implements [`TranscriptJournal`], so it plugs directly into
/// `Agent::open_session_with_journal`.
pub struct SessionJournal {
    path: PathBuf,
    session_id: String,
    /// The open file (holding the `flock` for the journal's lifetime) **and** the next
    /// envelope `seq`, behind one mutex. The counter lives inside the lock on purpose: a
    /// line's `seq` is chosen, rendered and written while the lock is held (fsynced here too
    /// when it is a sync point, ADR 0040), and the counter only advances once the write
    /// succeeded. So `seq` order is file order even when the runtime's events and the core's
    /// journal records are appended concurrently.
    inner: Arc<Mutex<Writer>>,
    /// Holds host appends back while a core replacement segment is open (ADR 0018 §A).
    gate: Arc<SegmentGate>,
    /// Computes the fee for one usage record at append time (ADR 0027). The host
    /// installs it; with none set (a test journal, a host with no catalog price) the record
    /// carries no fee, which an old reader treats as an old record.
    usage_fee: std::sync::OnceLock<UsageFeeFn>,
    /// Test seam: pause one in-segment core append so a driver test can observe a segment
    /// mid-flight and prove no other writer deadlocks on it.
    #[cfg(feature = "testing")]
    segment_pause: Arc<SegmentPause>,
}

/// What the writer mutex guards. Counting starts at 1 because [`SessionJournal::create`]
/// already wrote `SessionCreated` at seq 0.
struct Writer {
    file: File,
    next_seq: u64,
    /// Bytes written through this writer; it equals the file length while this writer is the
    /// only one holding the lock.
    written_len: u64,
    /// A byte copy of every committed line, sent after a successful `write_all` for the
    /// `radian exec --output jsonl` stdout stream. Installed under the same lock that every
    /// write takes, so installing it also reports the exact prefix length the reader must
    /// take from the file. The send is unbounded and its failure is dropped: a blocked or
    /// gone stdout must never stall or fail a journal write.
    tee: Option<UnboundedSender<Vec<u8>>>,
    /// The file length at the last successful fsync — the durable prefix. Every record written
    /// after it is only `write`+`flush`ed and is carried by the next sync point (ADR 0040).
    synced_len: u64,
    /// Test-only: how many fsyncs this writer has performed (the per-step count assertion).
    #[cfg(feature = "testing")]
    syncs: u64,
    /// Test-only: how many writes (attempted appends) this writer has seen.
    #[cfg(feature = "testing")]
    writes: u64,
    /// Test-only: the crash seam. The write whose index reaches this behaves as if the system
    /// crashed: the unsynced bytes are dropped and the write fails (ADR 0040). It fires at a
    /// chosen **record** boundary, not only at a sync point, so a crash can be injected
    /// between any two consecutive sync points.
    #[cfg(feature = "testing")]
    crash_at: Option<u64>,
    /// Test-only: whether the crash seam has fired at least once.
    #[cfg(feature = "testing")]
    crash_fired: bool,
}

impl Writer {
    /// A writer over `file` whose durable prefix is `written_len` bytes (0 for a fresh file,
    /// the committed length for a resume). Counting starts at 1 because `create` has already
    /// written `SessionCreated` at seq 0.
    fn new(file: File, next_seq: u64, written_len: u64) -> Self {
        Self {
            file,
            next_seq,
            written_len,
            tee: None,
            synced_len: written_len,
            #[cfg(feature = "testing")]
            syncs: 0,
            #[cfg(feature = "testing")]
            writes: 0,
            #[cfg(feature = "testing")]
            crash_at: None,
            #[cfg(feature = "testing")]
            crash_fired: false,
        }
    }
}

impl SessionJournal {
    /// Creates a new session file: takes the exclusive lock, truncates any torn tail, and
    /// writes `SessionCreated` as seq 0. A file that already contains complete lines is
    /// refused rather than overwritten.
    pub fn create(
        path: impl AsRef<Path>,
        session_id: &str,
        created: SessionCreatedData,
    ) -> Result<Self, WriteError> {
        let path = path.as_ref().to_path_buf();
        let mut file = Self::open_locked(&path)?;
        let size = file.metadata().map_err(|e| io_err(&path, e))?.len();
        if size > 0 {
            if scan_lines(&mut file, &path)?.is_some() {
                return Err(WriteError::NotEmpty { path });
            }
            // Only a torn tail: truncate to zero and start the file over.
            file.set_len(0).map_err(|e| io_err(&path, e))?;
        }
        let journal = SessionJournal {
            path: path.clone(),
            session_id: session_id.to_owned(),
            inner: Arc::new(Mutex::new(Writer::new(file, 0, 0))),
            gate: Arc::new(SegmentGate::new()),
            usage_fee: std::sync::OnceLock::new(),
            #[cfg(feature = "testing")]
            segment_pause: Arc::new(SegmentPause::default()),
        };
        journal.write_line(EVT_SESSION_CREATED, &CreatedData(&created))?;
        Ok(journal)
    }

    /// Opens an existing session file for appending: takes the exclusive lock, truncates
    /// the torn tail (ADR 0006 §3.2), verifies `schema_version`/`session_id` from the
    /// file itself, and continues `seq` after the last complete line. This is the entry a
    /// host uses for repair write-back (ADR 0002 E9) and for resuming after a crash.
    pub fn resume(path: impl AsRef<Path>, session_id: &str) -> Result<Self, WriteError> {
        let lock = Self::lock_for_resume(path)?;
        let parsed = lock.parse()?;
        lock.into_journal(session_id, &parsed)
            .map(|(journal, _)| journal)
    }

    /// Takes the single-writer lock for a resume **before** anything reads the file
    /// (resume-perf B2). A resume that parsed before locking could hold a `committed_bytes`
    /// from before a concurrent appender's commit and then `set_len` truncate that appender's
    /// committed lines away. The caller parses under the lock ([`ResumeLock::parse`]) and
    /// builds the appending journal from that parse ([`ResumeLock::into_journal`]).
    pub fn lock_for_resume(path: impl AsRef<Path>) -> Result<ResumeLock, WriteError> {
        let path = path.as_ref().to_path_buf();
        #[cfg(feature = "testing")]
        crate::testing::resume_before_lock(&path);
        let file = Self::open_locked(&path)?;
        Ok(ResumeLock { path, file })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Installs the fee recorder (ADR 0027): the writer calls it for every
    /// `ModelUsage`/`CompactionUsage` record and writes the returned fee beside the usage. Set
    /// once, before the session runs; a second call is ignored.
    pub fn set_usage_fee(&self, fee: UsageFeeFn) {
        let _ = self.usage_fee.set(fee);
    }

    /// Installs the byte tee for `radian exec --output jsonl` and returns the file length at
    /// that moment. The caller prints the file's first that many bytes itself (the lines
    /// written before the tee existed) and then prints what the tee sends; because install
    /// and every write take the same lock, no line is missed or printed twice.
    pub fn set_tee(&self, sender: UnboundedSender<Vec<u8>>) -> u64 {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let len = writer.written_len;
        writer.tee = Some(sender);
        len
    }

    /// The `seq` the next append will use.
    pub fn next_seq(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
    }

    /// Marks the session closed (ADR 0040 decision 2: close is a sync point): every record
    /// written so far is made durable, and the fsync error, if any, is returned. Unlike
    /// [`Drop`], which runs the same sync best-effort, this surfaces the failure. It does not
    /// refuse later appends; a later append re-establishes its own sync points.
    pub fn close(&self) -> Result<(), WriteError> {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        writer.file.sync_all().map_err(|e| io_err(&self.path, e))?;
        writer.synced_len = writer.written_len;
        Ok(())
    }

    /// Appends a runtime event (ADR 0006 §5.5–§5.7) through the same lock, `seq` counter and
    /// fsync as every other line. `critical` comes from the event type (ADR 0006 §4), never
    /// from the caller, and transcript events cannot be written this way — they go through
    /// [`TranscriptJournal::append`], which keeps `MessageAppended.index` contiguous.
    pub async fn append_runtime_event(&self, event: RuntimeEvent) -> Result<(), WriteError> {
        let name = event.name();
        let critical = event.critical();
        let data = raw_data(&runtime_data(&event)).map_err(|e| invalid_data(&self.path, e))?;
        // ADR 0018 §A: a runtime event must not land inside a core replacement segment.
        // Wait for the segment to close, then re-check under the writer lock so a segment
        // that opened between the wait and the lock cannot be entered.
        loop {
            self.gate
                .wait_closed()
                .await
                .map_err(|()| WriteError::SegmentPoisoned {
                    path: self.path.clone(),
                })?;
            let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if self.gate.is_open() {
                drop(writer);
                continue;
            }
            let result = write_locked(&mut writer, &self.session_id, (name, critical), &data);
            drop(writer);
            return result.map_err(|e| io_err(&self.path, e));
        }
    }

    /// Test seam: whether a core replacement segment is currently open.
    #[cfg(feature = "testing")]
    pub fn segment_open(&self) -> bool {
        self.gate.is_open()
    }

    /// Test seam: completes once a host append is waiting on an open segment.
    #[cfg(feature = "testing")]
    pub async fn wait_for_gate_waiter(&self) {
        self.gate.wait_for_waiter().await;
    }

    /// Test seam: pause the next in-segment core append (the replacement commit) on a
    /// `Notify` until [`SessionJournal::release_segment_append`].
    #[cfg(feature = "testing")]
    pub fn pause_next_segment_append(&self) {
        self.segment_pause.arm();
    }

    /// Test seam: completes once the paused in-segment append has reached its checkpoint.
    #[cfg(feature = "testing")]
    pub async fn wait_segment_paused(&self) {
        self.segment_pause.wait_paused().await;
    }

    /// Test seam: release the paused in-segment append.
    #[cfg(feature = "testing")]
    pub fn release_segment_append(&self) {
        self.segment_pause.release();
    }

    fn open_locked(path: &Path) -> Result<File, WriteError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            // O_APPEND (implies write): every write lands at the current end, so after a
            // torn-tail truncation the next append cannot leave a hole where the torn
            // bytes were.
            .append(true)
            .create(true)
            // We never truncate on open; the torn tail is handled explicitly by
            // `create`/`resume` after scanning, so an existing session is never lost.
            .truncate(false);
        // A new session file is owner-only (ADR 0013): the JSONL can carry a model-echoed
        // secret in a `provider_payload` (ADR 0006 §11). The mode applies only when the file
        // is created; an existing file keeps the mode the user gave it (we only warn).
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(crate::fs::PRIVATE_FILE_MODE);
        }
        let file = options.open(path).map_err(|e| WriteError::Open {
            path: path.to_path_buf(),
            source: e,
        })?;
        crate::fs::warn_if_wide(path);
        Self::lock_exclusive(&file, path)?;
        Ok(file)
    }

    /// Takes the exclusive writer lock, retrying a contended `flock` for [`LOCK_RETRY_BUDGET`].
    ///
    /// # Why a false `Locked` is possible
    ///
    /// `flock` follows the **open file description**, not the process. On macOS and Linux a
    /// `fork` copies the parent's descriptor table, so a child shares the parent's open file
    /// description — and therefore its lock — for the whole `[fork, exec)` window, only
    /// releasing it at `exec` (when the descriptor's `FD_CLOEXEC` closes it). A host that has
    /// a session journal open, forks a child in another thread (a `git` helper, a tool
    /// subprocess), and closes then reopens the journal inside that window sees the child's
    /// inherited descriptor still holding the lock, though no second writer exists.
    ///
    /// The retry turns that transient window back into success. The numbers, on this machine:
    /// 8 threads closing and reopening a journal while 4 threads spawn `git --version` saw
    /// 13966 false `Locked` in 24000 reopens before this retry, and 0 after. A real second
    /// writer holds the lock for as long as it lives, far beyond the budget, and is still
    /// refused with [`WriteError::Locked`].
    fn lock_exclusive(file: &File, path: &Path) -> Result<(), WriteError> {
        let locked = || WriteError::Locked {
            path: path.to_path_buf(),
        };
        let deadline = Instant::now() + LOCK_RETRY_BUDGET;
        let mut wait = LOCK_RETRY_FIRST;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(()),
                // `fs2` surfaces the raw `flock` errno, so contention is `EWOULDBLOCK`.
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    #[cfg(feature = "testing")]
                    LOCK_RETRY_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(locked());
                    }
                    std::thread::sleep(wait.min(deadline - now));
                    wait = wait.saturating_mul(2);
                }
                // Any other failure is not a transient window; refuse immediately.
                Err(_) => return Err(locked()),
            }
        }
    }

    /// Writes one line, choosing its `seq` under the writer lock (see [`SessionJournal`]).
    fn write_line<D: Serialize>(&self, event: (&str, bool), data: &D) -> Result<(), WriteError> {
        let data = raw_data(data).map_err(|e| invalid_data(&self.path, e))?;
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        write_locked(&mut writer, &self.session_id, event, &data).map_err(|e| io_err(&self.path, e))
    }

    /// Test-only: appends a message synchronously, so a test can play a concurrent appender
    /// without an async runtime (resume-perf B2's lock-order test).
    #[cfg(feature = "testing")]
    pub fn test_append_message(
        &self,
        index: u64,
        message: agent_core::Message,
    ) -> Result<(), WriteError> {
        self.write_line(
            EVT_MESSAGE_APPENDED,
            &serde_json::json!({ "index": index, "message": message }),
        )
    }
}

impl Drop for SessionJournal {
    /// ADR 0040 decision 2: a session close is a sync point, but `Drop` cannot await or report
    /// a failure, and every record written since the last sync point might not be durable yet.
    /// So the last handle makes the file durable best-effort; a host that wants the error uses
    /// [`SessionJournal::close`]. Runs once, when the last `Arc<SessionJournal>` goes away
    /// (after any in-flight append has released the lock).
    fn drop(&mut self) {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if writer.file.sync_all().is_ok() {
            writer.synced_len = writer.written_len;
        }
    }
}

/// Test-only observability and fault injection for the group-commit timing (ADR 0040).
#[cfg(feature = "testing")]
impl SessionJournal {
    /// How many fsyncs this journal has performed. A test counts the delta across a turn to
    /// assert the group-commit timing (a step should fsync 2–3 times, not once per record).
    pub fn sync_count(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).syncs
    }

    /// The durable prefix length: the file length at the last successful fsync. Everything
    /// past it has been `write`+`flush`ed but is not yet durable. Equals the file length after
    /// a sync point, `close`, or `Drop`.
    pub fn durable_len(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .synced_len
    }

    /// How many writes (attempted appends) this journal has seen. A test measures the delta
    /// across a turn to enumerate a crash at every record boundary.
    pub fn write_count(&self) -> u64 {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).writes
    }

    /// Simulates a system crash: truncates the file to [`SessionJournal::durable_len`], dropping
    /// every byte written after the last fsync, and makes the truncation durable. The caller
    /// then drops the journal and reopens with `resume`.
    pub fn simulate_system_crash(&self) -> std::io::Result<()> {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        truncate_to_synced(&mut writer)
    }

    /// Arms the mid-turn crash seam: the `n`-th write after arming (1-based) acts as if the
    /// system crashed — it drops the unsynced bytes and fails, and every later write fails the
    /// same way. `n` is a record index, so a crash can be injected between any two consecutive
    /// sync points. The turn then ends like a crashed session.
    pub fn crash_at_write(&self, n: u64) {
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        writer.crash_at = Some(writer.writes + n.max(1));
    }

    /// Whether the crash seam has fired at least once since the journal opened.
    pub fn crash_fired(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .crash_fired
    }
}

/// A held single-writer lock on a session file being resumed, from
/// [`SessionJournal::lock_for_resume`]. The lock is held from before the parse until
/// [`ResumeLock::into_journal`] builds the appending journal, so nothing can append between
/// the parse and the `set_len` that removes a torn tail.
pub struct ResumeLock {
    path: PathBuf,
    file: File,
}

impl ResumeLock {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Parses the file **while the lock is held**. This is the one parse a resume needs: the
    /// journal open, the driver's recovery and the host's initial view all reuse it.
    pub fn parse(&self) -> Result<crate::reader::Session, WriteError> {
        Ok(crate::reader::Session::open(&self.path)?)
    }

    /// Builds the appending journal from `parsed`. Returns whether an aborted replacement
    /// segment was written, so a caller that shares the parse knows the fold on disk changed.
    pub fn into_journal(
        self,
        session_id: &str,
        parsed: &crate::reader::Session,
    ) -> Result<(SessionJournal, bool), WriteError> {
        let ResumeLock { path, file } = self;
        let Some(last_seq) = parsed.last_seq() else {
            return Err(WriteError::Empty { path });
        };
        if parsed.schema_version() > SCHEMA_VERSION {
            return Err(ReadError::SchemaTooNew {
                path: path.clone(),
                found: parsed.schema_version(),
                supported: SCHEMA_VERSION,
            }
            .into());
        }
        if parsed.schema_version() != SCHEMA_VERSION {
            return Err(ReadError::UnsupportedSchema {
                path: path.clone(),
                found: parsed.schema_version(),
            }
            .into());
        }
        if parsed.session_id() != session_id {
            return Err(ReadError::Envelope {
                path: path.clone(),
                line: 1,
                message: format!(
                    "declared session_id {:?}, expected {session_id:?}",
                    parsed.session_id()
                ),
            }
            .into());
        }
        // ADR 0019 §6/§7: validate before touching the file. `Session::open` ignores a torn
        // tail and folds the committed prefix, so a corrupt file is refused with its bytes
        // exactly as they were — the torn tail is not truncated first. An already-aborted
        // segment is closed, so a second `resume` does not repeat the abort.
        let abort_revision = parsed
            .uncommitted_replacements()
            .iter()
            .rev()
            .find(|segment| segment.end == agent_core::ReplacementEnd::EndOfFile)
            .map(|segment| segment.revision);
        // Only now that the file is known sound is the torn tail (ADR 0006 §3.2) removed:
        // everything past the committed prefix of the parse we made under this lock.
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        if size > parsed.committed_bytes() {
            file.set_len(parsed.committed_bytes())
                .map_err(|e| io_err(&path, e))?;
        }
        let journal = SessionJournal {
            path,
            session_id: session_id.to_owned(),
            inner: Arc::new(Mutex::new(Writer::new(
                file,
                last_seq + 1,
                parsed.committed_bytes(),
            ))),
            gate: Arc::new(SegmentGate::new()),
            usage_fee: std::sync::OnceLock::new(),
            #[cfg(feature = "testing")]
            segment_pause: Arc::new(SegmentPause::default()),
        };
        let mut wrote_abort = false;
        if let Some(revision) = abort_revision {
            journal.write_line(
                EVT_TRANSCRIPT_REPLACEMENT_ABORTED,
                &AbortedData { revision },
            )?;
            wrote_abort = true;
        }
        Ok((journal, wrote_abort))
    }
}

/// The event body serialized once, outside the lock. `RawValue` keeps it verbatim, so
/// `OpaqueJson` payloads inside `Message`/`ModelUsage` still reach the file byte-for-byte;
/// a detour through `serde_json::Value` would re-serialize (and reorder) them.
fn raw_data<D: Serialize>(data: &D) -> serde_json::Result<Box<RawValue>> {
    RawValue::from_string(serde_json::to_string(data)?)
}

/// Picks the next `seq`, renders the envelope and writes it — all while the caller holds the
/// writer lock (ADR 0006 §15.3). Every line is `write`+`flush`ed; only a sync point
/// (`must_sync`, ADR 0040 decision 2) is `fsync`ed, and only then does the group of records
/// written since the last fsync become durable. The counter advances after a successful write:
/// `write`+`flush` for a non-sync record, `write`+`flush`+`fsync` for a sync record. A failed
/// write — or a failed fsync at a sync point — never burns a `seq` number.
fn write_locked(
    writer: &mut Writer,
    session_id: &str,
    (event, critical): (&str, bool),
    data: &RawValue,
) -> std::io::Result<()> {
    // A test-only crash seam (ADR 0040): on the armed write index the process is treated as
    // dead here — the unsynced bytes are dropped and the write fails, so the caller's turn
    // ends exactly as a system crash would leave it. It is sticky: every later write fails the
    // same way, so the file stays at the truncated prefix.
    #[cfg(feature = "testing")]
    {
        writer.writes += 1;
        if let Some(at) = writer.crash_at
            && writer.writes >= at
        {
            writer.crash_fired = true;
            truncate_to_synced(writer)?;
            return Err(std::io::Error::other("injected: system crash"));
        }
    }
    // A test-only seam: fail before any byte is written, so the file stays untouched.
    #[cfg(feature = "testing")]
    if take_write_failure(session_id, event) {
        return Err(std::io::Error::other("injected: the write failed"));
    }
    let seq = writer.next_seq;
    let line = Line {
        schema_version: SCHEMA_VERSION,
        session_id,
        seq,
        ts: now_utc(),
        event,
        critical,
        data,
    };
    let mut bytes = serde_json::to_vec(&line)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    writer.file.write_all(&bytes)?;
    writer.file.flush()?;
    // The tee carries committed bytes only: a failed write (including the injected crash
    // above, which returns before any byte is written) sends nothing.
    if let Some(tee) = &writer.tee {
        let _ = tee.send(bytes.clone());
    }
    writer.written_len += bytes.len() as u64;
    if must_sync(event) {
        writer.file.sync_all()?;
        writer.synced_len = writer.written_len;
        #[cfg(feature = "testing")]
        {
            writer.syncs += 1;
        }
    }
    writer.next_seq = seq + 1;
    Ok(())
}

/// Drops every byte written past the last successful fsync, as a system crash would (power
/// loss / kernel panic). The truncation itself is made durable so a later reopen in the same
/// process sees the same file. Test-only.
#[cfg(feature = "testing")]
fn truncate_to_synced(writer: &mut Writer) -> std::io::Result<()> {
    writer.file.set_len(writer.synced_len)?;
    writer.file.sync_all()?;
    writer.written_len = writer.synced_len;
    Ok(())
}

/// Test seam (ADR 0018 §A): pause one in-segment core append on a `Notify`. The driver
/// deadlock test observes a replacement segment mid-flight this way. Race-free against an
/// early `release`: the `paused`/`released` flags are checked with the `Notified` future
/// already enabled.
#[cfg(feature = "testing")]
#[derive(Default)]
pub(crate) struct SegmentPause {
    armed: std::sync::atomic::AtomicBool,
    paused: std::sync::atomic::AtomicBool,
    released: std::sync::atomic::AtomicBool,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[cfg(feature = "testing")]
impl SegmentPause {
    pub(crate) fn arm(&self) {
        self.released
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) async fn checkpoint(&self) {
        if !self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
        self.reached.notify_waiters();
        loop {
            let notified = self.release.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .released
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }
            notified.await;
        }
        self.paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) async fn wait_paused(&self) {
        loop {
            let notified = self.reached.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.paused.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

/// Test-only fault seam: arm a one-shot failure for the next write of `event` **in the
/// session `session_id`**, so an integration test can see the error path without touching the
/// file. Keyed by session so tests running in parallel in one process cannot consume each
/// other's armed failure. Compiled only with the `testing` feature.
#[cfg(feature = "testing")]
pub fn fail_next_write(session_id: &str, event: &'static str) {
    FAIL_NEXT_WRITE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((session_id.to_owned(), event));
}

/// Test-only fault seam (ADR 0019 §7): the next `TranscriptReplacementAborted` write in the
/// session `session_id` fails.
#[cfg(feature = "testing")]
pub fn fail_next_aborted_write(session_id: &str) {
    fail_next_write(session_id, EVT_TRANSCRIPT_REPLACEMENT_ABORTED.0);
}

#[cfg(feature = "testing")]
static FAIL_NEXT_WRITE: std::sync::Mutex<Vec<(String, &'static str)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(feature = "testing")]
fn take_write_failure(session_id: &str, event: &str) -> bool {
    let mut armed = FAIL_NEXT_WRITE.lock().unwrap_or_else(|e| e.into_inner());
    match armed
        .iter()
        .position(|(sid, armed_event)| sid == session_id && *armed_event == event)
    {
        Some(index) => {
            armed.remove(index);
            true
        }
        None => false,
    }
}

/// Test-only observability (feature `testing`): how many times the writer lock has been
/// retried because it was contended. A test that deliberately leaves the lock held (e.g. by
/// a forked child that inherited the descriptor) waits for this to advance before it frees
/// the lock, so it knows `open_locked` is provably in its retry loop rather than racing the
/// first attempt.
#[cfg(feature = "testing")]
static LOCK_RETRY_ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only seam: the value of [`LOCK_RETRY_ATTEMPTS`]. See [`SessionJournal::open_locked`].
#[cfg(feature = "testing")]
pub fn lock_retry_attempts() -> u64 {
    LOCK_RETRY_ATTEMPTS.load(std::sync::atomic::Ordering::SeqCst)
}

fn invalid_data(path: &Path, e: serde_json::Error) -> WriteError {
    WriteError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
    }
}

impl TranscriptJournal for SessionJournal {
    fn append(&self, record: JournalRecord) -> BoxFuture<'static, Result<(), JournalError>> {
        // Render the body now (it borrows `record`); the `seq` is chosen later, under the
        // lock, so it matches file order even when runtime events are appended concurrently.
        // The fee is computed here too: the route in force is read once, at the moment the
        // core appends the record, so the journal and the live footer agree (ADR 0027
        // revision 12).
        let fee = usage_of(&record)
            .and_then(|usage| self.usage_fee.get().and_then(|fee_for| fee_for(usage)));
        let rendered = render_record(&record, fee)
            .ok_or_else(|| "this record has no disk event".to_string())
            .and_then(|(event, data)| {
                raw_data(&data)
                    .map(|data| (event, data))
                    .map_err(|e| e.to_string())
            });
        let path = self.path.clone();
        let session_id = self.session_id.clone();
        let inner = Arc::clone(&self.inner);
        let gate = Arc::clone(&self.gate);
        let effect = gate_effect(&record);
        #[cfg(feature = "testing")]
        let pause = matches!(record, JournalRecord::TranscriptReplacementCommitted { .. })
            .then(|| Arc::clone(&self.segment_pause));
        Box::pin(async move {
            #[cfg(feature = "testing")]
            if let Some(pause) = pause {
                pause.checkpoint().await;
            }
            let (event, data) = rendered.map_err(JournalError)?;
            let res = tokio::task::spawn_blocking(move || {
                let mut writer = inner.lock().unwrap_or_else(|e| e.into_inner());
                // D3: the core awaits this future and never cancels it; the fsync inside is
                // what makes the append durable before the next transcript change may start.
                let result = write_locked(&mut writer, &session_id, event, &data);
                // ADR 0018 §A: observe the segment boundaries under the same lock that chose
                // the line's place, so a host append cannot slip inside a failed segment.
                match (&result, effect) {
                    (Ok(()), GateEffect::Begin(revision)) => gate.begin(revision),
                    (Ok(()), GateEffect::Close(revision)) => gate.close(revision),
                    (Err(_), _) if gate.is_open() => gate.poison(),
                    _ => {}
                }
                result
            })
            .await
            .map_err(|join| {
                JournalError(format!("{}: append task failed: {join}", path.display()))
            })?;
            res.map_err(|e| JournalError(format!("{}: append failed: {e}", path.display())))
        })
    }
}

/// The envelope is written with the fixed field order of ADR 0006 §3, which is exactly
/// this struct's declaration order. `data` stays a typed, borrowed view, so `OpaqueJson`
/// payloads inside `Message`/`ModelUsage` pass through byte-for-byte.
#[derive(Serialize)]
struct Line<'a> {
    schema_version: u64,
    session_id: &'a str,
    seq: u64,
    ts: String,
    event: &'a str,
    critical: bool,
    data: &'a RawValue,
}

#[derive(Serialize)]
struct CreatedData<'a>(&'a SessionCreatedData);

#[derive(Serialize)]
struct MessageData<'a> {
    index: usize,
    message: &'a Message,
}

#[derive(Serialize)]
struct AbortedData {
    revision: u64,
}

#[derive(Serialize)]
struct ReplacedData {
    revision: u64,
    len: usize,
}

#[derive(Serialize)]
struct StartedData<'a> {
    call_id: &'a str,
    name: &'a str,
    arguments: &'a Value,
}

#[derive(Serialize)]
struct SettledData<'a> {
    call_id: &'a str,
    result: &'a ToolResult,
}

#[derive(Serialize)]
struct UsageData<'a> {
    usage: &'a ModelUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    fee: Option<UsageFee>,
}

#[derive(Serialize)]
struct CompactionUsageData<'a> {
    revision: u64,
    model: &'a str,
    usage: &'a ModelUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    fee: Option<UsageFee>,
}

#[derive(Serialize)]
struct ContextCompactedData<'a> {
    revision: u64,
    source_revision: u64,
    source_len: usize,
    summary: &'a str,
    first_kept: usize,
    tokens_before: u64,
    tokens_after: u64,
    below_target: bool,
    model: &'a str,
    /// ADR 0018, decision 3: written only for the deterministic fallback; absent for
    /// a model summary (a forward reader defaults it to `false`).
    #[serde(skip_serializing_if = "is_false")]
    deterministic: bool,
}

/// `skip_serializing_if` for the deterministic flag: omit it when `false`.
fn is_false(value: &bool) -> bool {
    !*value
}

/// How one core record changes the replacement-segment gate (ADR 0018 §A).
#[derive(Debug, Clone, Copy)]
enum GateEffect {
    None,
    Begin(u64),
    Close(u64),
}

fn gate_effect(record: &JournalRecord) -> GateEffect {
    match record {
        JournalRecord::TranscriptReplaced { revision, .. } => GateEffect::Begin(*revision),
        JournalRecord::TranscriptReplacementCommitted { revision, .. }
        | JournalRecord::TranscriptReplacementAborted { revision } => GateEffect::Close(*revision),
        _ => GateEffect::None,
    }
}

/// The usage a journal record carries, or `None` for a record without any (ADR 0027 revision
/// 12: only the two usage records are priced).
fn usage_of(record: &JournalRecord) -> Option<&ModelUsage> {
    match record {
        JournalRecord::ModelUsage { usage } | JournalRecord::CompactionUsage { usage, .. } => {
            Some(usage)
        }
        _ => None,
    }
}

/// Maps a core record to its disk event name/`critical` and its `data` shape (ADR 0006
/// §12). Every current variant is handled; `JournalRecord` is `#[non_exhaustive]`, so a
/// future variant falls into the `None` arm and the append fails loudly instead of being
/// dropped silently.
fn render_record(
    record: &JournalRecord,
    fee: Option<UsageFee>,
) -> Option<((&'static str, bool), Render<'_>)> {
    let (event, data) = match record {
        JournalRecord::Message { index, message } => (
            EVT_MESSAGE_APPENDED,
            Render::Message(MessageData {
                index: *index,
                message,
            }),
        ),
        JournalRecord::TranscriptReplaced { revision, len } => (
            EVT_TRANSCRIPT_REPLACED,
            Render::Replaced(ReplacedData {
                revision: *revision,
                len: *len,
            }),
        ),
        JournalRecord::TranscriptReplacementCommitted { revision, len } => (
            EVT_TRANSCRIPT_REPLACEMENT_COMMITTED,
            Render::Replaced(ReplacedData {
                revision: *revision,
                len: *len,
            }),
        ),
        JournalRecord::TranscriptReplacementAborted { revision } => (
            EVT_TRANSCRIPT_REPLACEMENT_ABORTED,
            Render::Aborted(AbortedData {
                revision: *revision,
            }),
        ),
        JournalRecord::ToolExecutionStarted {
            call_id,
            name,
            arguments,
        } => (
            EVT_TOOL_STARTED,
            Render::Started(StartedData {
                call_id,
                name,
                arguments,
            }),
        ),
        JournalRecord::ToolExecutionSettled { call_id, result } => (
            EVT_TOOL_SETTLED,
            Render::Settled(SettledData { call_id, result }),
        ),
        JournalRecord::ModelUsage { usage } => (EVT_USAGE, Render::Usage(UsageData { usage, fee })),
        JournalRecord::CompactionUsage {
            revision,
            model,
            usage,
        } => (
            EVT_COMPACTION_USAGE,
            Render::CompactionUsage(CompactionUsageData {
                revision: *revision,
                model,
                usage,
                fee,
            }),
        ),
        JournalRecord::ContextCompacted {
            revision,
            source_revision,
            source_len,
            summary,
            first_kept,
            tokens_before,
            tokens_after,
            below_target,
            model,
            deterministic,
        } => (
            EVT_CONTEXT_COMPACTED,
            Render::ContextCompacted(ContextCompactedData {
                revision: *revision,
                source_revision: *source_revision,
                source_len: *source_len,
                summary,
                first_kept: *first_kept,
                tokens_before: *tokens_before,
                tokens_after: *tokens_after,
                below_target: *below_target,
                model,
                deterministic: *deterministic,
            }),
        ),
        _ => return None,
    };
    Some((event, data))
}

enum Render<'a> {
    Message(MessageData<'a>),
    Replaced(ReplacedData),
    Aborted(AbortedData),
    Started(StartedData<'a>),
    Settled(SettledData<'a>),
    Usage(UsageData<'a>),
    CompactionUsage(CompactionUsageData<'a>),
    ContextCompacted(ContextCompactedData<'a>),
}

impl Serialize for Render<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Render::Message(d) => d.serialize(serializer),
            Render::Replaced(d) => d.serialize(serializer),
            Render::Aborted(d) => d.serialize(serializer),
            Render::Started(d) => d.serialize(serializer),
            Render::Settled(d) => d.serialize(serializer),
            Render::Usage(d) => d.serialize(serializer),
            Render::CompactionUsage(d) => d.serialize(serializer),
            Render::ContextCompacted(d) => d.serialize(serializer),
        }
    }
}

/// The current UTC time in the same format the JSONL envelope uses. Public so a host can
/// stamp an adjacent sidecar file (e.g. the TUI's panic log) with the session's own clock.
pub fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

/// Whether a live writer holds the session file's exclusive lock — i.e. a session process is
/// running (ADR 0043 D14: `radian attach` uses this to tell a live session's open tool calls
/// from a crash's synthesised results).
///
/// Read-only and non-blocking: the file is opened for reading and a **shared** lock is tried.
/// A shared lock is granted only while no writer holds the exclusive one, so two readers
/// probing at the same time both succeed (neither is mistaken for a writer), and the probe
/// never blocks a session and never writes. A file a writer holds returns `WouldBlock`; a
/// missing file is not live.
pub fn session_is_live(path: impl AsRef<Path>) -> bool {
    let Ok(file) = File::open(path.as_ref()) else {
        return false;
    };
    // Fully qualified: Rust 1.89+ added inherent `File::try_lock_shared`/`unlock`, which would
    // otherwise shadow the `fs2` trait at the project's 1.88 MSRV and return a different error
    // type. `fs2` is the writer's own locking implementation.
    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        // `fs2` surfaces the raw `flock` errno, so an exclusive holder is `EWOULDBLOCK`.
        Err(e) if e.kind() == ErrorKind::WouldBlock => true,
        // Any other error: do not claim a session that is not there.
        Err(_) => false,
    }
}

fn io_err(path: &Path, source: std::io::Error) -> WriteError {
    WriteError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// An envelope with `data` left as a `Value`: used only by the resume scan, which needs
/// `seq`/`session_id`/`schema_version` and nothing more.
#[derive(Deserialize)]
struct ScanEnvelope {
    schema_version: u64,
    session_id: String,
    seq: u64,
}

/// Streams the file line by line (never loading it whole) to validate every line and learn
/// the last `seq` (`None` for a file with no complete line). `create` uses it to refuse a file
/// that already holds a session.
fn scan_lines(file: &mut File, path: &Path) -> Result<Option<u64>, WriteError> {
    file.seek(SeekFrom::Start(0)).map_err(|e| io_err(path, e))?;
    let mut version = 0u64;
    let mut sid: Option<String> = None;
    let mut last_seq: Option<u64> = None;
    let mut line: Vec<u8> = Vec::new();
    let mut lineno: u64 = 0;
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).map_err(|e| WriteError::Scan {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        let mut start = 0;
        for i in 0..n {
            if buf[i] == b'\n' {
                line.extend_from_slice(&buf[start..=i]);
                let raw = &line[..line.len() - 1];
                // Every LF-terminated line must parse, including an empty one. A line that
                // fails to parse is corruption, and the append path must refuse it too
                // (ADR 0006 §3.2-3). Skipping an empty line here let `resume` append after
                // corruption and produce a file the reader then refuses forever.
                lineno += 1;
                let envelope: ScanEnvelope =
                    serde_json::from_slice(raw).map_err(|e| ReadError::Corrupt {
                        path: path.to_path_buf(),
                        line: lineno,
                        column: e.column(),
                        message: e.to_string(),
                    })?;
                if lineno == 1 {
                    version = envelope.schema_version;
                    sid = Some(envelope.session_id.clone());
                } else if envelope.schema_version != version
                    || envelope.session_id != sid.as_deref().unwrap_or_default()
                {
                    return Err(WriteError::Policy(ReadError::Envelope {
                        path: path.to_path_buf(),
                        line: lineno,
                        message: "schema_version or session_id changed mid-file".into(),
                    }));
                }
                last_seq = Some(envelope.seq);
                line.clear();
                start = i + 1;
            }
        }
        if start < n {
            line.extend_from_slice(&buf[start..]);
        }
    }
    Ok(last_seq)
}

#[cfg(test)]
mod tests {
    use super::{SessionCreatedData, SessionJournal, must_sync, session_is_live};
    use crate::runtime::{RuntimeEvent, TurnOutcome};

    fn tee_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "radian-tee-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_changed(revision: u64) -> RuntimeEvent {
        RuntimeEvent::ConfigChanged {
            revision,
            change: serde_json::json!({"approval": "auto"}),
        }
    }

    /// The `radian exec --output jsonl` byte tee: installing it after some lines reports the
    /// exact prefix length, and every line written after the install arrives on the tee
    /// byte-identical to the file's suffix — no line lost, none doubled. Mutation: delete
    /// the `tee.send` in `write_locked` → the tee suffix is empty and this fails.
    #[tokio::test]
    async fn the_tee_replays_the_suffix_byte_identically() {
        let dir = tee_dir("suffix");
        let path = dir.join("tee.jsonl");
        let journal = SessionJournal::create(
            &path,
            "tee",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        // Lines before the install: `SessionCreated` plus two runtime events.
        journal.append_runtime_event(config_changed(1)).await.unwrap();
        journal
            .append_runtime_event(RuntimeEvent::TurnStarted {
                turn_id: "t1".into(),
            })
            .await
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let prefix_len = journal.set_tee(tx) as usize;
        // Lines after the install.
        journal
            .append_runtime_event(RuntimeEvent::TurnFinished {
                turn_id: "t1".into(),
                outcome: TurnOutcome::Completed,
                reason: None,
                stop_reason: None,
            })
            .await
            .unwrap();
        journal.append_runtime_event(config_changed(2)).await.unwrap();
        drop(journal);
        let mut teed = Vec::new();
        while let Some(bytes) = rx.recv().await {
            teed.extend_from_slice(&bytes);
        }
        let file = std::fs::read(&path).unwrap();
        let (prefix, suffix) = file.split_at(prefix_len);
        // The prefix is whole lines: it ends on a line boundary and holds the three lines
        // written before the install.
        assert!(prefix.ends_with(b"\n"), "the prefix must end on a line boundary");
        assert_eq!(prefix.iter().filter(|b| **b == b'\n').count(), 3);
        // The tee carries the suffix exactly: the two lines written after the install.
        assert_eq!(suffix, teed.as_slice());
        assert_eq!(suffix.iter().filter(|b| **b == b'\n').count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed write sends nothing to the tee (the injected crash returns before any byte
    /// is written). Mutation: move the `tee.send` above the crash seam → this fails.
    #[tokio::test]
    async fn a_failed_write_sends_nothing_to_the_tee() {
        let dir = tee_dir("failed");
        let path = dir.join("tee.jsonl");
        let journal = SessionJournal::create(
            &path,
            "tee",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        journal.set_tee(tx);
        journal.crash_at_write(1);
        assert!(journal.append_runtime_event(config_changed(1)).await.is_err());
        drop(journal);
        assert!(
            rx.recv().await.is_none(),
            "a failed write must send nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR 0040 decision 2's sync set is exactly these names; a name added to or removed from
    /// the set is a decision change, so it is pinned here as well as behaviorally.
    #[test]
    fn the_sync_set_is_exactly_the_boundaries() {
        let sync = [
            "SessionCreated",
            "ToolExecutionStarted",
            "TurnFinished",
            "TurnFailed",
            "TranscriptReplacementCommitted",
            "TranscriptReplacementAborted",
        ];
        let non_sync = [
            "InboxEnqueued",
            "InputClaimed",
            "InputAdmitted",
            "TurnStarted",
            "MessageAppended",
            "ToolExecutionSettled",
            "TranscriptReplaced",
            "ConfigChanged",
            "ContextCompacted",
            "CompactionUsageRecorded",
            "Rewound",
            "ContextBudgetReached",
            "PermissionResolved",
            "ApprovalPolicyChanged",
            "MemoryInjected",
            "ModelUsageRecorded",
            "SessionForked",
            "ProjectTrustChanged",
            "UserBash",
            "BackgroundJobStarted",
            "BackgroundJobSettled",
        ];
        for name in sync {
            assert!(must_sync(name), "{name} must be a sync point");
        }
        for name in non_sync {
            assert!(!must_sync(name), "{name} must not be a sync point");
        }
    }

    /// The liveness probe (ADR 0043 D14): a `SessionJournal` holds the writer's exclusive lock
    /// for its lifetime, so the probe reports live while it exists and not-live after it is
    /// dropped. The probe takes only a **shared** lock and releases it at once, so it never
    /// blocks the writer and repeated probes agree.
    #[test]
    fn the_liveness_probe_follows_the_writer_lock() {
        let dir = std::env::temp_dir().join(format!("radian-live-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("live.jsonl");

        assert!(!session_is_live(&path), "a missing file is not live");
        let journal = SessionJournal::create(
            &path,
            "live",
            SessionCreatedData::new("/w", serde_json::json!({})),
        )
        .unwrap();
        assert!(session_is_live(&path), "the writer holds the lock");
        assert!(
            session_is_live(&path),
            "the probe released its shared lock, so a second probe agrees"
        );
        drop(journal);
        assert!(
            !session_is_live(&path),
            "the lock is released with the journal"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
