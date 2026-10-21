//! The append-only JSONL log: one JSONL file per scope, with the same mechanics as
//! `radian-session` (ADR 0006 §2.2/§7) but its own event schema (ADR 0021 D3).
//!
//! - a single-writer `flock` for the file's lifetime;
//! - every append is `write` + `flush` + `fsync` before it returns;
//! - a torn tail (bytes after the last LF) is ignored and truncated before the next append;
//! - a malformed line refuses the log with its 1-based line number;
//! - an unknown `critical` event refuses the log, an unknown non-critical one is skipped.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{MemoryError, PurgeStep};
use crate::fs::{PRIVATE_FILE_MODE, create_private_file, ensure_private_dir};
use crate::model::now_utc;

/// The memory schema version this crate writes and accepts.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// One accepted envelope line.
#[derive(Debug, Clone)]
pub(crate) struct LoggedEvent {
    pub line: u64,
    pub event: String,
    pub data: Value,
}

/// How many times a racing replacement is tolerated before `open_locked` gives up. A replace
/// (Purge) and an open never loop forever together in practice.
const MAX_OPEN_RETRIES: usize = 64;

// Test seam: a callback run after `open()` and before the writer lock is taken, so a test
// can replace the file at `path` in exactly that window. Thread-local, so a test never
// affects another one; production never sets it.
thread_local! {
    static OPEN_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Installs the open-hook callback (one shot, on this thread). Used only by tests.
#[doc(hidden)]
pub fn set_open_hook(hook: Box<dyn FnOnce()>) {
    OPEN_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
}

fn run_open_hook() {
    if let Some(hook) = OPEN_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

/// One JSONL log file plus its writer lock and running `seq`.
pub(crate) struct Log {
    path: PathBuf,
    file: File,
    next_seq: u64,
}

impl Log {
    /// Opens (creating if needed) the log at `path`, takes the single-writer lock, drops a
    /// torn tail, and returns the accepted events in file order.
    pub(crate) fn open(
        path: &Path,
        known: &[&str],
    ) -> Result<(Self, Vec<LoggedEvent>), MemoryError> {
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent).map_err(|e| MemoryError::io(parent, e))?;
        }
        let mut file = open_locked(path)?;
        let (events, committed, next_seq) = scan(&mut file, path, known)?;
        let len = file.metadata().map_err(|e| MemoryError::io(path, e))?.len();
        if len > committed {
            // A torn tail: drop it before any append so a new line never follows a half one.
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

    /// Opens an **existing** log without creating anything. `None` when the file is absent,
    /// so a read-only operation never leaves an empty scope behind.
    pub(crate) fn open_existing(
        path: &Path,
        known: &[&str],
    ) -> Result<Option<(Self, Vec<LoggedEvent>)>, MemoryError> {
        if !path.exists() {
            return Ok(None);
        }
        Log::open(path, known).map(Some)
    }

    /// Appends one event, choosing its `seq` under the lock and fsyncing before returning.
    pub(crate) fn append(
        &mut self,
        name: &str,
        critical: bool,
        data: &Value,
    ) -> Result<(), MemoryError> {
        let seq = self.next_seq;
        let line = Line {
            schema_version: SCHEMA_VERSION,
            seq,
            ts: now_utc(),
            event: name,
            critical,
            data,
        };
        let mut bytes = serde_json::to_vec(&line).map_err(|e| invalid(&self.path, e))?;
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
        self.next_seq = seq + 1;
        Ok(())
    }

    /// Rewrites the log atomically (ADR 0021 D3): write the new content to a temporary file in
    /// the same directory, fsync it, rename over the log, fsync the directory. A crash at any
    /// step leaves the path holding either the old log or the new one.
    ///
    /// `fault` stops after the named step to simulate that crash; it is a test seam.
    pub(crate) fn purge(
        &mut self,
        lines: &[(String, bool, Value)],
        fault: Option<PurgeStep>,
    ) -> Result<(), MemoryError> {
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        let temp = temp_path(&self.path);
        {
            let mut file = create_private_file(&temp).map_err(|e| MemoryError::io(&temp, e))?;
            for (index, (name, critical, data)) in lines.iter().enumerate() {
                let line = Line {
                    schema_version: SCHEMA_VERSION,
                    seq: index as u64,
                    ts: now_utc(),
                    event: name,
                    critical: *critical,
                    data,
                };
                let mut bytes = serde_json::to_vec(&line).map_err(|e| invalid(&temp, e))?;
                bytes.push(b'\n');
                file.write_all(&bytes)
                    .map_err(|e| MemoryError::io(&temp, e))?;
            }
            if fault == Some(PurgeStep::TempWritten) {
                return Ok(());
            }
            file.sync_all().map_err(|e| MemoryError::io(&temp, e))?;
            if fault == Some(PurgeStep::TempFsynced) {
                return Ok(());
            }
        }
        std::fs::rename(&temp, &self.path).map_err(|e| MemoryError::io(&self.path, e))?;
        if fault == Some(PurgeStep::Renamed) {
            return Ok(());
        }
        fsync_dir(dir)?;
        if fault == Some(PurgeStep::DirFsynced) {
            return Ok(());
        }
        // The old fd points at the replaced (now unlinked) inode; reopen the path and take
        // the lock on the new file. The rename-and-reopen window is tiny and a competing
        // writer surfaces as `Locked`, never as a half-written file.
        let fresh = open_locked(&self.path)?;
        self.file = fresh;
        self.next_seq = lines.len() as u64;
        Ok(())
    }
}

/// Opens the log and takes the single-writer lock, **revalidating the inode** afterwards.
///
/// A writer that reached `open()` before a Purge renamed a new file over the path is holding
/// the *old*, now-unlinked inode; once it acquires the lock there it would replay the old
/// content and fsync a write into a file with no path. So after locking, the file's
/// `(dev, ino)` is compared with the path's current `(dev, ino)`; on any mismatch (or a
/// vanished path) the descriptor is dropped and the open+lock is retried.
///
/// Non-Unix platforms skip the comparison (the memory store targets Unix), so the race is not
/// closed there.
fn open_locked(path: &Path) -> Result<File, MemoryError> {
    for _ in 0..MAX_OPEN_RETRIES {
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        let file = options.open(path).map_err(|e| MemoryError::io(path, e))?;
        run_open_hook();
        // Blocking: two writers serialize on the lock (ADR 0021 D4), so a concurrent create
        // surfaces as KeyConflict/RevisionConflict rather than as a lock error.
        file.lock_exclusive().map_err(|_| MemoryError::Locked {
            path: path.to_path_buf(),
        })?;
        if same_file(&file, path) {
            return Ok(file);
        }
    }
    Err(MemoryError::Raced {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
fn same_file(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(open), Ok(current)) = (file.metadata(), std::fs::metadata(path)) else {
        return false;
    };
    open.dev() == current.dev() && open.ino() == current.ino()
}

#[cfg(not(unix))]
fn same_file(_file: &File, _path: &Path) -> bool {
    true
}

/// Streams the log, applying the §3.2/§7 rules. Returns the accepted events and the committed
/// byte length (everything through the last LF).
fn scan(
    file: &mut File,
    path: &Path,
    known: &[&str],
) -> Result<(Vec<LoggedEvent>, u64, u64), MemoryError> {
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
            // No LF: the trailing bytes are a torn tail and are not committed (ADR 0006 §2.2).
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
            // An unknown non-critical event is skipped, not committed to the state, but its
            // line is part of the committed prefix.
            continue;
        }
        events.push(LoggedEvent {
            line: lineno,
            event: envelope.event,
            data: envelope.data,
        });
    }
    // `expect_seq` counts every committed line, including skipped unknown events, so it is
    // the next free seq.
    Ok((events, committed, expect_seq))
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "log".into());
    path.with_file_name(format!(".{name}.tmp-{}", std::process::id()))
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<(), MemoryError> {
    let handle = File::open(dir).map_err(|e| MemoryError::io(dir, e))?;
    handle.sync_all().map_err(|e| MemoryError::io(dir, e))
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<(), MemoryError> {
    Ok(())
}

fn invalid(path: &Path, source: serde_json::Error) -> MemoryError {
    MemoryError::io(
        path,
        std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    )
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
