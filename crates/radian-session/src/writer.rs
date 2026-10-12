//! The write side: a real, fsyncing [`agent_core::TranscriptJournal`] on a JSONL file.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::{JournalError, JournalRecord, Message, ModelUsage, ToolResult, TranscriptJournal};
use fs2::FileExt;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ReadError;

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
const EVT_TOOL_STARTED: (&str, bool) = ("ToolExecutionStarted", true);
const EVT_TOOL_SETTLED: (&str, bool) = ("ToolExecutionSettled", true);
const EVT_USAGE: (&str, bool) = ("ModelUsageRecorded", false);

/// One JSONL session file plus its single-writer lock and running `seq` counter. It
/// implements [`TranscriptJournal`], so it plugs directly into
/// `Agent::open_session_with_journal`.
pub struct SessionJournal {
    path: PathBuf,
    session_id: String,
    /// The next envelope `seq`. Counting starts at 1 because [`SessionJournal::create`]
    /// already wrote `SessionCreated` at seq 0.
    next_seq: AtomicU64,
    /// The open file, holding the `flock` for the journal's lifetime. Writes are also
    /// serialized through this mutex, so even a misused concurrent `append` cannot
    /// interleave two lines.
    inner: Arc<Mutex<File>>,
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
            next_seq: AtomicU64::new(1),
            inner: Arc::new(Mutex::new(file)),
        };
        journal.write_line_numbered(EVT_SESSION_CREATED, 0, &CreatedData(&created))?;
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
        if scan.version != SCHEMA_VERSION {
            return Err(ReadError::Envelope {
                path: path.clone(),
                line: 1,
                message: format!(
                    "declared schema_version {}, expected {SCHEMA_VERSION}",
                    scan.version
                ),
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
        if scan.torn {
            file.set_len(scan.offset_after_last_lf)
                .map_err(|e| io_err(&path, e))?;
        }
        Ok(SessionJournal {
            path,
            session_id: session_id.to_owned(),
            next_seq: AtomicU64::new(last_seq + 1),
            inner: Arc::new(Mutex::new(file)),
        })
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The `seq` the next append will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    fn open_locked(path: &Path) -> Result<File, WriteError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            // We never truncate on open; the torn tail is handled explicitly by
            // `create`/`resume` after scanning, so an existing session is never lost.
            .truncate(false)
            .open(path)
            .map_err(|e| WriteError::Open {
                path: path.to_path_buf(),
                source: e,
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(file),
            Err(_) => Err(WriteError::Locked {
                path: path.to_path_buf(),
            }),
        }
    }

    fn render_line<D: Serialize>(
        &self,
        (event, critical): (&str, bool),
        seq: u64,
        data: &D,
    ) -> Result<Vec<u8>, WriteError> {
        let line = Line {
            schema_version: SCHEMA_VERSION,
            session_id: &self.session_id,
            seq,
            ts: now_utc(),
            event,
            critical,
            data,
        };
        let mut bytes = serde_json::to_vec(&line).map_err(|e| WriteError::Io {
            path: self.path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        })?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn write_line_numbered<D: Serialize>(
        &self,
        event: (&str, bool),
        seq: u64,
        data: &D,
    ) -> Result<(), WriteError> {
        let line = self.render_line(event, seq, data)?;
        let mut file = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        file.write_all(&line)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_all())
            .map_err(|e| io_err(&self.path, e))
    }
}

impl TranscriptJournal for SessionJournal {
    fn append(&self, record: JournalRecord) -> BoxFuture<'static, Result<(), JournalError>> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let rendered = render_record(&record)
            .ok_or_else(|| "this record has no disk event".to_string())
            .and_then(|(event, data)| {
                self.render_line(event, seq, &data)
                    .map_err(|e| e.to_string())
            });
        let path = self.path.clone();
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let line = rendered.map_err(JournalError)?;
            let res = tokio::task::spawn_blocking(move || {
                let mut file = inner.lock().unwrap_or_else(|e| e.into_inner());
                file.write_all(&line)?;
                file.flush()?;
                // D3: the core awaits this future and never cancels it; the fsync is what
                // makes the append durable before the next transcript change may start.
                file.sync_all()
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
struct Line<'a, D: Serialize> {
    schema_version: u64,
    session_id: &'a str,
    seq: u64,
    ts: String,
    event: &'a str,
    critical: bool,
    data: &'a D,
}

#[derive(Serialize)]
struct CreatedData<'a>(&'a SessionCreatedData);

#[derive(Serialize)]
struct MessageData<'a> {
    index: usize,
    message: &'a Message,
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
    Started(StartedData<'a>),
    Settled(SettledData<'a>),
    Usage(UsageData<'a>),
}

impl Serialize for Render<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Render::Message(d) => d.serialize(serializer),
            Render::Replaced(d) => d.serialize(serializer),
            Render::Started(d) => d.serialize(serializer),
            Render::Settled(d) => d.serialize(serializer),
            Render::Usage(d) => d.serialize(serializer),
        }
    }
}

fn now_utc() -> String {
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
                if !raw.is_empty() {
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
                }
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
