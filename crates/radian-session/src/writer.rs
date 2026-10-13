//! The write side: a real, fsyncing [`agent_core::TranscriptJournal`] on a JSONL file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::{JournalError, JournalRecord, Message, ModelUsage, ToolResult, TranscriptJournal};
use fs2::FileExt;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::ReadError;
use crate::runtime::{RuntimeEvent, runtime_data};

/// The schema version this crate writes and accepts.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// What a new session file's `SessionCreated` line carries. `workspace` and
/// `config_snapshot` are required by ADR 0006 §5.1; the reserved optional fields
/// (`agent_id`/`profile_ref`/`workspace_binding`/`parent`) are Phase 6 additions and are
/// not exposed yet.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCreatedData {
    pub workspace: String,
    pub config_snapshot: Value,
}

impl SessionCreatedData {
    pub fn new(workspace: impl Into<String>, config_snapshot: Value) -> Self {
        Self {
            workspace: workspace.into(),
            config_snapshot,
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

/// One JSONL session file plus its single-writer lock and running `seq` counter. It
/// implements [`TranscriptJournal`], so it plugs directly into
/// `Agent::open_session_with_journal`.
pub struct SessionJournal {
    path: PathBuf,
    session_id: String,
    /// The open file (holding the `flock` for the journal's lifetime) **and** the next
    /// envelope `seq`, behind one mutex. The counter lives inside the lock on purpose: a
    /// line's `seq` is chosen, rendered, written and fsynced while the lock is held, and the
    /// counter only advances once the write succeeded. So `seq` order is file order even when
    /// the runtime's events and the core's journal records are appended concurrently.
    inner: Arc<Mutex<Writer>>,
}

/// What the writer mutex guards. Counting starts at 1 because [`SessionJournal::create`]
/// already wrote `SessionCreated` at seq 0.
struct Writer {
    file: File,
    next_seq: u64,
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
            let scan = scan_lines(&mut file, &path)?;
            if scan.last_seq.is_some() {
                return Err(WriteError::NotEmpty { path });
            }
            // Only a torn tail: truncate to zero and start the file over.
            file.set_len(0).map_err(|e| io_err(&path, e))?;
        }
        let journal = SessionJournal {
            path: path.clone(),
            session_id: session_id.to_owned(),
            inner: Arc::new(Mutex::new(Writer { file, next_seq: 0 })),
        };
        journal.write_line(EVT_SESSION_CREATED, &CreatedData(&created))?;
        Ok(journal)
    }

    /// Opens an existing session file for appending: takes the exclusive lock, truncates
    /// the torn tail (ADR 0006 §3.2), verifies `schema_version`/`session_id` from the
    /// file itself, and continues `seq` after the last complete line. This is the entry a
    /// host uses for repair write-back (ADR 0002 E9) and for resuming after a crash.
    pub fn resume(path: impl AsRef<Path>, session_id: &str) -> Result<Self, WriteError> {
        let path = path.as_ref().to_path_buf();
        let mut file = Self::open_locked(&path)?;
        let scan = scan_lines(&mut file, &path)?;
        let Some(last_seq) = scan.last_seq else {
            return Err(WriteError::Empty { path });
        };
        if scan.version > SCHEMA_VERSION {
            return Err(ReadError::SchemaTooNew {
                path: path.clone(),
                found: scan.version,
                supported: SCHEMA_VERSION,
            }
            .into());
        }
        if scan.version != SCHEMA_VERSION {
            return Err(ReadError::UnsupportedSchema {
                path: path.clone(),
                found: scan.version,
            }
            .into());
        }
        if scan.sid.as_deref() != Some(session_id) {
            return Err(ReadError::Envelope {
                path: path.clone(),
                line: 1,
                message: format!(
                    "declared session_id {:?}, expected {session_id:?}",
                    scan.sid.unwrap_or_default()
                ),
            }
            .into());
        }
        // ADR 0019 §6/§7: validate before touching the file. `Session::open` ignores a torn
        // tail and folds the committed prefix, so a corrupt file is refused with its bytes
        // exactly as they were — the torn tail is not truncated first. An already-aborted
        // segment is closed, so a second `resume` does not repeat the abort.
        let parsed = crate::reader::Session::open(&path)?;
        let abort_revision = parsed
            .uncommitted_replacements()
            .iter()
            .rev()
            .find(|segment| segment.end == agent_core::ReplacementEnd::EndOfFile)
            .map(|segment| segment.revision);
        // Only now that the file is known sound is the torn tail (ADR 0006 §3.2) removed.
        if scan.torn {
            file.set_len(scan.offset_after_last_lf)
                .map_err(|e| io_err(&path, e))?;
        }
        let journal = SessionJournal {
            path,
            session_id: session_id.to_owned(),
            inner: Arc::new(Mutex::new(Writer {
                file,
                next_seq: last_seq + 1,
            })),
        };
        if let Some(revision) = abort_revision {
            journal.write_line(
                EVT_TRANSCRIPT_REPLACEMENT_ABORTED,
                &AbortedData { revision },
            )?;
        }
        Ok(journal)
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `seq` the next append will use.
    pub fn next_seq(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .next_seq
    }

    /// Appends a runtime event (ADR 0006 §5.5–§5.7) through the same lock, `seq` counter and
    /// fsync as every other line. `critical` comes from the event type (ADR 0006 §4), never
    /// from the caller, and transcript events cannot be written this way — they go through
    /// [`TranscriptJournal::append`], which keeps `MessageAppended.index` contiguous.
    pub async fn append_runtime_event(&self, event: RuntimeEvent) -> Result<(), WriteError> {
        self.write_line((event.name(), event.critical()), &runtime_data(&event))
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
        match file.try_lock_exclusive() {
            Ok(()) => Ok(file),
            Err(_) => Err(WriteError::Locked {
                path: path.to_path_buf(),
            }),
        }
    }

    /// Writes one line, choosing its `seq` under the writer lock (see [`SessionJournal`]).
    fn write_line<D: Serialize>(&self, event: (&str, bool), data: &D) -> Result<(), WriteError> {
        let data = raw_data(data).map_err(|e| invalid_data(&self.path, e))?;
        let mut writer = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        write_locked(&mut writer, &self.session_id, event, &data).map_err(|e| io_err(&self.path, e))
    }
}

/// The event body serialized once, outside the lock. `RawValue` keeps it verbatim, so
/// `OpaqueJson` payloads inside `Message`/`ModelUsage` still reach the file byte-for-byte;
/// a detour through `serde_json::Value` would re-serialize (and reorder) them.
fn raw_data<D: Serialize>(data: &D) -> serde_json::Result<Box<RawValue>> {
    RawValue::from_string(serde_json::to_string(data)?)
}

/// Picks the next `seq`, renders the envelope, writes it, flushes and fsyncs — all while the
/// caller holds the writer lock — and advances the counter only after the line is durable.
/// A failed write therefore never burns a `seq` number.
fn write_locked(
    writer: &mut Writer,
    session_id: &str,
    (event, critical): (&str, bool),
    data: &RawValue,
) -> std::io::Result<()> {
    // A test-only seam: fail before any byte is written, so the file stays untouched.
    #[cfg(feature = "testing")]
    if take_aborted_write_failure(event) {
        return Err(std::io::Error::other("injected: the abort write failed"));
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
    writer.file.sync_all()?;
    writer.next_seq = seq + 1;
    Ok(())
}

/// Test-only fault seam (ADR 0019 §7): arm a one-shot failure for the next
/// `TranscriptReplacementAborted` write, so an integration test can see `resume` surface
/// the error without touching the file. Compiled only with the `testing` feature.
#[cfg(feature = "testing")]
pub fn fail_next_aborted_write() {
    FAIL_NEXT_ABORTED_WRITE.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(feature = "testing")]
static FAIL_NEXT_ABORTED_WRITE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "testing")]
fn take_aborted_write_failure(event: &str) -> bool {
    event == EVT_TRANSCRIPT_REPLACEMENT_ABORTED.0
        && FAIL_NEXT_ABORTED_WRITE.swap(false, std::sync::atomic::Ordering::SeqCst)
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
        let rendered = render_record(&record)
            .ok_or_else(|| "this record has no disk event".to_string())
            .and_then(|(event, data)| {
                raw_data(&data)
                    .map(|data| (event, data))
                    .map_err(|e| e.to_string())
            });
        let path = self.path.clone();
        let session_id = self.session_id.clone();
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let (event, data) = rendered.map_err(JournalError)?;
            let res = tokio::task::spawn_blocking(move || {
                let mut writer = inner.lock().unwrap_or_else(|e| e.into_inner());
                // D3: the core awaits this future and never cancels it; the fsync inside is
                // what makes the append durable before the next transcript change may start.
                write_locked(&mut writer, &session_id, event, &data)
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
}

/// Maps a core record to its disk event name/`critical` and its `data` shape (ADR 0006
/// §12). Every current variant is handled; `JournalRecord` is `#[non_exhaustive]`, so a
/// future variant falls into the `None` arm and the append fails loudly instead of being
/// dropped silently.
fn render_record(record: &JournalRecord) -> Option<((&'static str, bool), Render<'_>)> {
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
        JournalRecord::ModelUsage { usage } => (EVT_USAGE, Render::Usage(UsageData { usage })),
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

struct Scan {
    version: u64,
    sid: Option<String>,
    last_seq: Option<u64>,
    offset_after_last_lf: u64,
    torn: bool,
}

/// Streams the file line by line (never loading it whole) to learn the declared
/// schema/session, the last `seq`, and where the last completed line ends so a torn tail
/// can be truncated away.
fn scan_lines(file: &mut File, path: &Path) -> Result<Scan, WriteError> {
    file.seek(SeekFrom::Start(0)).map_err(|e| io_err(path, e))?;
    let mut version = 0u64;
    let mut sid: Option<String> = None;
    let mut last_seq: Option<u64> = None;
    let mut offset_after_last_lf: u64 = 0;
    let mut offset: u64 = 0;
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
                offset_after_last_lf = offset + i as u64 + 1;
                line.clear();
                start = i + 1;
            }
        }
        if start < n {
            line.extend_from_slice(&buf[start..]);
        }
        offset += n as u64;
    }
    Ok(Scan {
        version,
        sid,
        last_seq,
        offset_after_last_lf,
        torn: !line.is_empty(),
    })
}
