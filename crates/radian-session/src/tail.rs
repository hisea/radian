//! The incremental, **read-only** session reader (ADR 0023 U6).
//!
//! [`Session::open`](crate::Session::open) reads and folds a whole file. A long-lived observer
//! (the TUI's projections) instead wants to notice appended events without re-parsing the
//! prefix. [`JournalTail`] keeps the per-line parser state (expected `seq`, the
//! [`TranscriptFold`](agent_core::TranscriptFold), the schema/session id, the skipped list)
//! and reads only the bytes appended since the last poll.
//!
//! It never takes the writer's lock and never writes: no torn-tail truncation, no
//! `TranscriptReplacementAborted`. Every validation rule — envelope, `seq` continuity, a
//! critical unknown event, the ADR 0019 pending-segment rules — is the **same** [`LineScanner`]
//! the one-shot scan uses, so an incremental reader and a fresh `Session::open` agree by
//! construction.
//!
//! There is no file watcher (no inotify/kqueue): the caller polls on its own cadence or when
//! its own wake-up fires.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use agent_core::Message;

use crate::error::ReadError;
use crate::reader::{EventData, LineScanner, MAX_LINE_BYTES, SessionEvent};

/// The result of one [`JournalTail::poll`].
#[derive(Debug, Clone)]
pub struct TailUpdate {
    /// The file was replaced (a new inode) or truncated; the reader started over from byte 0.
    pub reset: bool,
    /// The accepted events appended since the last poll, in order.
    pub events: Vec<SessionEvent>,
    /// The current effective history (ADR 0019/0002): committed replacements in effect,
    /// uncommitted segments dropped, unpaired calls repaired.
    pub history: Vec<Message>,
    /// True when `history` may have changed since the last poll (a transcript record or a
    /// reset). The history itself is the authority; this is only a cheap hint.
    pub history_changed: bool,
}

/// A file's identity, so a `rename` of a new file over the path is noticed. `dev`/`ino` on
/// Unix; a no-op on other platforms (the workspace targets Unix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    const ZERO: Self = Self { dev: 0, ino: 0 };
}

fn file_id(meta: &std::fs::Metadata) -> FileId {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileId::ZERO
    }
}

/// An incremental cursor over one session file.
pub struct JournalTail {
    path: PathBuf,
    /// The end of the committed prefix read so far.
    offset: u64,
    scanner: LineScanner,
    identity: FileId,
    /// The display text of the first corruption, so every later poll keeps failing instead of
    /// skipping the bad line and reading on.
    broken: Option<String>,
}

impl JournalTail {
    /// Opens `path` read-only and reads its committed prefix, validating it exactly as
    /// [`Session::open`](crate::Session::open) would. A writer in another process holding the
    /// file's lock does not matter: this never locks and never writes.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReadError> {
        let path = path.as_ref().to_path_buf();
        let mut tail = Self {
            path,
            offset: 0,
            scanner: LineScanner::new(),
            identity: FileId::ZERO,
            broken: None,
        };
        tail.poll()?;
        Ok(tail)
    }

    /// Reads the complete lines appended since the last poll and folds them.
    ///
    /// A partial trailing line (no LF) is a torn tail: it is not parsed, the offset does not
    /// move, and the next poll reads it once complete. A new inode or a shorter file resets the
    /// reader to byte 0 (`TailUpdate::reset`). A corrupt complete line returns the same error a
    /// fresh `Session::open` would, and every later poll returns an error rather than skipping
    /// it.
    pub fn poll(&mut self) -> Result<TailUpdate, ReadError> {
        if let Some(text) = &self.broken {
            return Err(ReadError::Corrupt {
                path: self.path.clone(),
                line: 0,
                column: 0,
                message: format!("the session file was already reported corrupt: {text}"),
            });
        }
        // Open first, then take the identity from the **opened** file: a `metadata(path)` taken
        // before the open could describe a file that a rename has replaced in between, and the
        // old offset would then be applied to the new file.
        let mut file = File::open(&self.path).map_err(|source| ReadError::Open {
            path: self.path.clone(),
            source,
        })?;
        let meta = file.metadata().map_err(|source| ReadError::Open {
            path: self.path.clone(),
            source,
        })?;
        let identity = file_id(&meta);
        let reset = identity != self.identity || meta.len() < self.offset;
        if reset {
            self.offset = 0;
            self.scanner = LineScanner::new();
        }
        self.identity = identity;

        let events =
            match read_new_lines(&mut file, &self.path, &mut self.offset, &mut self.scanner) {
                Ok(events) => events,
                Err(error) => {
                    self.broken = Some(error.to_string());
                    return Err(error);
                }
            };
        let history = self.scanner.recovered_history();
        let history_changed = reset || events.iter().any(is_history_event);
        Ok(TailUpdate {
            reset,
            events,
            history,
            history_changed,
        })
    }

    /// The path being tailed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The end of the committed prefix read so far.
    pub fn offset(&self) -> u64 {
        self.offset
    }
}

/// Whether an event can change the folded history (a message, a replacement boundary or a
/// tool marker that `repair` uses). Usage and runtime events cannot.
fn is_history_event(event: &SessionEvent) -> bool {
    matches!(
        &event.data,
        EventData::MessageAppended { .. }
            | EventData::TranscriptReplaced { .. }
            | EventData::TranscriptReplacementCommitted { .. }
            | EventData::TranscriptReplacementAborted { .. }
            | EventData::ToolExecutionStarted { .. }
            | EventData::ToolExecutionSettled { .. }
    )
}

/// Reads complete lines from `offset` on, feeding each to `scanner`. The offset advances only
/// past complete lines; a torn tail leaves it alone.
fn read_new_lines(
    file: &mut File,
    path: &Path,
    offset: &mut u64,
    scanner: &mut LineScanner,
) -> Result<Vec<SessionEvent>, ReadError> {
    file.seek(SeekFrom::Start(*offset))
        .map_err(|source| ReadError::Open {
            path: path.to_path_buf(),
            source,
        })?;
    let mut reader = BufReader::new(file);
    let mut line: Vec<u8> = Vec::new();
    let mut events = Vec::new();
    let mut line_offset = *offset;
    loop {
        line.clear();
        let n = {
            let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
            limited
                .read_until(b'\n', &mut line)
                .map_err(|source| ReadError::Open {
                    path: path.to_path_buf(),
                    source,
                })?
        };
        if n == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            if line.len() > MAX_LINE_BYTES {
                return Err(ReadError::Corrupt {
                    path: path.to_path_buf(),
                    line: scanner.lineno() + 1,
                    column: MAX_LINE_BYTES,
                    message: format!("line exceeds the {MAX_LINE_BYTES}-byte cap"),
                });
            }
            // No LF means no commit: a torn tail (ADR 0006 §3.2). Do not parse or advance.
            break;
        }
        let raw = std::str::from_utf8(&line[..line.len() - 1]).map_err(|e| ReadError::Corrupt {
            path: path.to_path_buf(),
            line: scanner.lineno() + 1,
            column: e.valid_up_to(),
            message: "line is not valid UTF-8".into(),
        })?;
        if let Some(event) = scanner.push::<ReadError>(raw, path, line_offset, n as u64)? {
            events.push(event);
        }
        line_offset += n as u64;
        *offset = line_offset;
    }
    Ok(events)
}
