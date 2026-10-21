//! The SQLite session index: a **disposable cache** over the authoritative JSONL files
//! (ADR 0006 §10). Nothing in it is a fact that cannot be re-derived from the JSONL; if it
//! is lost or found inconsistent, it is rebuilt by scanning the files, and the JSONL is
//! never modified.
//!
//! Every column except `sessions.path` and `sessions.indexed_at` is derived by the same
//! [`scan_stream`] the reader uses (ADR 0006 §8), so the LF-commit / torn-tail rule
//! (ADR 0006 §2.2) is shared by construction: a torn tail is neither indexed nor counted.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use agent_core::{ContentPart, Message};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::ReadError;
use crate::reader::{EventData, scan_stream};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    session_id     TEXT PRIMARY KEY,
    path           TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    workspace      TEXT,
    created_ts     TEXT,
    last_ts        TEXT,
    event_count    INTEGER NOT NULL,
    skipped_count  INTEGER NOT NULL,
    file_bytes     INTEGER NOT NULL,
    indexed_at     TEXT NOT NULL,
    -- The change signal for an incremental rebuild: the file's size and mtime when it was
    -- indexed. A file whose size and mtime both match is not re-scanned.
    file_size      INTEGER NOT NULL DEFAULT 0,
    file_mtime     INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS events (
    session_id    TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    ts            TEXT NOT NULL,
    event         TEXT NOT NULL,
    critical      INTEGER NOT NULL,
    byte_offset   INTEGER NOT NULL,
    line_len      INTEGER NOT NULL,
    message_index INTEGER,
    text          TEXT,
    PRIMARY KEY (session_id, seq)
);
"#;

/// Why an index operation failed. A session file that violates the format is a
/// [`IndexError::Session`]; the JSONL itself is left untouched by every operation here.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("session file: {0}")]
    Session(#[from] ReadError),
    #[error("{path}: {message}")]
    Empty { path: PathBuf, message: String },
    #[error("cannot read the original line: {0}")]
    Io(#[from] std::io::Error),
}

/// A summary of one indexed session, all derived from its JSONL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub path: PathBuf,
    pub schema_version: u64,
    pub workspace: Option<String>,
    pub created_ts: Option<String>,
    pub last_ts: Option<String>,
    pub event_count: u64,
    pub skipped_count: u64,
    /// The committed prefix length of the file (ADR 0006 §2.2).
    pub file_bytes: u64,
}

/// One indexed event row. `text` is the searchable visible text of a `MessageAppended`
/// (user/assistant text parts only) and `None` for every other event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedEvent {
    pub seq: u64,
    pub ts: String,
    pub event: String,
    pub critical: bool,
    pub byte_offset: u64,
    pub line_len: u64,
    pub message_index: Option<u64>,
    pub text: Option<String>,
}

/// A search hit. The `(session_id, seq)` pair plus `byte_offset`/`line_len` map back to the
/// exact original line in the authoritative file; use [`read_line_at`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub path: PathBuf,
    pub seq: u64,
    pub message_index: Option<u64>,
    pub byte_offset: u64,
    pub line_len: u64,
    pub snippet: String,
}

/// A file that could not be indexed; the rebuild continues and lists these instead of
/// failing the whole directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildWarning {
    pub path: PathBuf,
    pub message: String,
}

/// The result of comparing the index against one JSONL file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub consistent: bool,
    pub issues: Vec<String>,
}

/// The result of [`Index::reconcile`]: whether a rebuild happened, the mismatches that
/// triggered it, and any file error. The JSONL is never written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconcile {
    pub rebuilt: bool,
    pub issues: Vec<String>,
    pub warnings: Vec<RebuildWarning>,
}

/// The index database. Open one per process (SQLite handles the file locking).
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Opens (creating if needed) the index at `path`, in WAL mode with a busy timeout
    /// (plan Phase 1), and ensures the schema exists.
    ///
    /// The database file is created owner-only (ADR 0013); SQLite creates its `-wal`/`-shm`
    /// siblings with the database's own mode, so they inherit it. An existing wider file is
    /// only warned about, never changed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, IndexError> {
        let path = path.as_ref();
        crate::fs::ensure_private_file(path)?;
        let conn = Connection::open(path)?;
        // WAL lets readers run while a writer holds the write lock; the busy timeout makes
        // a contended writer wait instead of failing immediately.
        let mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        debug_assert_eq!(mode, "wal");
        conn.execute_batch("PRAGMA busy_timeout=5000;")?;
        conn.execute_batch(SCHEMA)?;
        // The index is a disposable cache: an older database predates the incremental
        // columns, so add them in place (a fresh database already has them).
        for column in ["file_size", "file_mtime"] {
            let present: bool = conn
                .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = ?1")?
                .exists([column])?;
            if !present {
                conn.execute_batch(&format!(
                    "ALTER TABLE sessions ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
                ))?;
            }
        }
        Ok(Self { conn })
    }

    /// The current `journal_mode` (tests assert WAL).
    pub fn journal_mode(&self) -> Result<String, IndexError> {
        Ok(self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?)
    }

    /// The current `busy_timeout` in milliseconds (tests assert it is set).
    pub fn busy_timeout(&self) -> Result<i64, IndexError> {
        Ok(self
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))?)
    }

    /// Rebuilds every `*.jsonl` in `dir` from the files. Files that violate the format are
    /// reported as warnings and skipped; the files themselves are never modified.
    pub fn rebuild_dir(
        &mut self,
        dir: impl AsRef<Path>,
    ) -> Result<Vec<RebuildWarning>, IndexError> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "jsonl") {
                paths.push(path);
            }
        }
        paths.sort();
        let warnings = self.rebuild(&paths)?;
        // A session whose file was deleted leaves the index (a disposable cache must not show
        // a session that is gone).
        let known: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT session_id, path FROM sessions")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.filter_map(|row| row.ok())
                .filter(|(_, path)| !paths.iter().any(|p| p.to_string_lossy() == *path))
                .map(|(id, _)| id)
                .collect()
        };
        for session_id in known {
            self.conn.execute(
                "DELETE FROM events WHERE session_id = ?1",
                params![session_id],
            )?;
            self.conn.execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                params![session_id],
            )?;
        }
        Ok(warnings)
    }

    /// Rebuilds the listed session files: replaces their index rows with a fresh scan of
    /// the JSONL. Never writes the JSONL. A file that cannot be parsed yields a warning and
    /// leaves its previous rows in place.
    pub fn rebuild(&mut self, paths: &[PathBuf]) -> Result<Vec<RebuildWarning>, IndexError> {
        self.rebuild_impl(paths, false)
    }

    /// [`Index::rebuild`] that re-scans every file, ignoring the size/mtime change signal.
    /// `reconcile` uses it: the audit already proved the index is wrong, so the file must be
    /// re-derived even when it has not changed.
    pub fn rebuild_forced(&mut self, paths: &[PathBuf]) -> Result<Vec<RebuildWarning>, IndexError> {
        self.rebuild_impl(paths, true)
    }

    fn rebuild_impl(
        &mut self,
        paths: &[PathBuf],
        force: bool,
    ) -> Result<Vec<RebuildWarning>, IndexError> {
        let tx = self.conn.transaction()?;
        let mut warnings = Vec::new();
        for path in paths {
            // A file whose size and mtime are unchanged since it was indexed is not
            // re-scanned (resume-perf-c): opening the picker used to re-read every session.
            let meta = match std::fs::metadata(path) {
                Ok(meta) => meta,
                Err(e) => {
                    warnings.push(RebuildWarning {
                        path: path.clone(),
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            let indexed: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT file_size, file_mtime FROM sessions WHERE path = ?1",
                    params![path.to_string_lossy()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if !force
                && let Some((size, mtime)) = indexed
                && size as u64 == meta.len()
                && mtime == mtime_nanos(&meta)
            {
                continue;
            }
            if let Err(e) = index_file(&tx, path) {
                warnings.push(RebuildWarning {
                    path: path.clone(),
                    message: e.to_string(),
                });
            }
        }
        tx.commit()?;
        Ok(warnings)
    }

    /// Sessions for one workspace (or every session when `workspace` is `None`), newest
    /// activity first. Reads only the index.
    pub fn list(&self, workspace: Option<&str>) -> Result<Vec<SessionSummary>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, path, schema_version, workspace, created_ts, last_ts, \
                    event_count, skipped_count, file_bytes \
             FROM sessions \
             WHERE (?1 IS NULL OR workspace = ?1) \
             ORDER BY last_ts DESC, session_id ASC",
        )?;
        let rows = stmt.query_map(params![workspace], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                schema_version: row.get::<_, i64>(2)? as u64,
                workspace: row.get(3)?,
                created_ts: row.get(4)?,
                last_ts: row.get(5)?,
                event_count: row.get::<_, i64>(6)? as u64,
                skipped_count: row.get::<_, i64>(7)? as u64,
                file_bytes: row.get::<_, i64>(8)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Every indexed event of one session, in `seq` order. Used by the derivation test and
    /// by callers that want the raw index view.
    pub fn indexed_events(&self, session_id: &str) -> Result<Vec<IndexedEvent>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, ts, event, critical, byte_offset, line_len, message_index, text \
             FROM events WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(IndexedEvent {
                seq: row.get::<_, i64>(0)? as u64,
                ts: row.get(1)?,
                event: row.get(2)?,
                critical: row.get::<_, i64>(3)? != 0,
                byte_offset: row.get::<_, i64>(4)? as u64,
                line_len: row.get::<_, i64>(5)? as u64,
                message_index: row.get::<_, Option<i64>>(6)?.map(|n| n as u64),
                text: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Substring search over the visible text of `MessageAppended` messages, restricted to
    /// one workspace. Reads only the index; `LIKE` with an escaped pattern (no FTS — see
    /// the crate docs / report). Results come back in `(session_id, seq)` order.
    pub fn search(
        &self,
        query: &str,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, IndexError> {
        let pattern = format!("%{}%", escape_like(query));
        let mut stmt = self.conn.prepare(
            "SELECT e.session_id, s.path, e.seq, e.message_index, e.byte_offset, e.line_len, e.text \
             FROM events e JOIN sessions s ON s.session_id = e.session_id \
             WHERE e.text IS NOT NULL AND e.text LIKE ?1 ESCAPE '\\' \
               AND (?2 IS NULL OR s.workspace = ?2) \
             ORDER BY e.session_id ASC, e.seq ASC \
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![pattern, workspace, limit as i64], |row| {
            let text: String = row.get(6)?;
            Ok(SearchHit {
                session_id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                seq: row.get::<_, i64>(2)? as u64,
                message_index: row.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                byte_offset: row.get::<_, i64>(4)? as u64,
                line_len: row.get::<_, i64>(5)? as u64,
                snippet: snippet(&text, query),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Compares the index against one JSONL file by re-scanning it. Any difference is a
    /// warning about the cache; the file is the authority.
    pub fn audit(&self, path: impl AsRef<Path>) -> Result<Audit, IndexError> {
        let path = path.as_ref();
        let path_str = path.to_string_lossy().to_string();
        let row = self
            .conn
            .query_row(
                "SELECT session_id, schema_version, workspace, created_ts, last_ts, \
                        event_count, skipped_count, file_bytes \
                 FROM sessions WHERE path = ?1",
                params![path_str],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)? as u64,
                        row.get::<_, i64>(6)? as u64,
                        row.get::<_, i64>(7)? as u64,
                    ))
                },
            )
            .optional()?;
        let Some((
            session_id,
            schema_version,
            workspace,
            created_ts,
            last_ts,
            count,
            skipped,
            bytes,
        )) = row
        else {
            return Ok(Audit {
                consistent: false,
                issues: vec![format!("{path_str} is not indexed")],
            });
        };

        let mut issues = Vec::new();
        let mut seen: u64 = 0;
        let mut file_workspace: Option<String> = None;
        let mut file_created: Option<String> = None;
        let mut file_last: Option<String> = None;
        let mut scan_error: Option<String> = None;
        {
            let mut stmt = self.conn.prepare(
                "SELECT ts, event, critical, byte_offset, line_len \
                 FROM events WHERE session_id = ?1 AND seq = ?2",
            )?;
            let scanned = scan_stream::<IndexError, _>(path, |event| {
                seen += 1;
                if event.event == "SessionCreated" {
                    file_created = Some(event.ts.clone());
                    if let EventData::Other(data) = &event.data {
                        file_workspace = data
                            .get("workspace")
                            .and_then(|w| w.as_str())
                            .map(str::to_owned);
                    }
                }
                file_last = Some(event.ts.clone());
                let indexed = stmt
                    .query_row(params![session_id, event.seq as i64], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)? != 0,
                            row.get::<_, i64>(3)? as u64,
                            row.get::<_, i64>(4)? as u64,
                        ))
                    })
                    .optional()?;
                match indexed {
                    None => issues.push(format!("seq {} is not indexed", event.seq)),
                    Some((ts, name, critical, offset, len)) => {
                        if ts != event.ts
                            || name != event.event
                            || critical != event.critical
                            || offset != event.byte_offset
                            || len != event.line_len
                        {
                            issues.push(format!(
                                "seq {}: index ({name} @{offset}+{len}) != file ({} @{}+{})",
                                event.seq, event.event, event.byte_offset, event.line_len
                            ));
                        }
                    }
                }
                Ok(())
            });
            match scanned {
                Ok(summary) => {
                    if summary.session_id != session_id {
                        issues.push(format!(
                            "session_id {} != file {}",
                            session_id, summary.session_id
                        ));
                    }
                    if summary.schema_version != schema_version {
                        issues.push("schema_version differs".into());
                    }
                    if summary.committed_bytes != bytes {
                        issues.push(format!(
                            "committed bytes {} != indexed {}",
                            summary.committed_bytes, bytes
                        ));
                    }
                    if summary.event_count != count {
                        issues.push(format!(
                            "event count {} != indexed {}",
                            summary.event_count, count
                        ));
                    }
                    if summary.skipped.len() as u64 != skipped {
                        issues.push("skipped count differs".into());
                    }
                    if file_workspace != workspace {
                        issues.push("workspace differs".into());
                    }
                    if file_created != created_ts {
                        issues.push("created_ts differs".into());
                    }
                    if file_last != last_ts {
                        issues.push("last_ts differs".into());
                    }
                }
                Err(e) => scan_error = Some(e.to_string()),
            }
        }
        if let Some(e) = scan_error {
            issues.push(format!("the file could not be re-scanned: {e}"));
        }
        // Extra indexed rows beyond the file (e.g. tampered or stale) are a mismatch too.
        let indexed_rows: u64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, i64>(0),
        )? as u64;
        if indexed_rows != seen {
            issues.push(format!(
                "indexed rows {indexed_rows} != scanned events {seen}"
            ));
        }

        Ok(Audit {
            consistent: issues.is_empty(),
            issues,
        })
    }

    /// Audits one file and, if the index disagrees with it, rebuilds that file's rows from
    /// the JSONL. The JSONL is the authority and is never written.
    pub fn reconcile(&mut self, path: impl AsRef<Path>) -> Result<Reconcile, IndexError> {
        let path = path.as_ref().to_path_buf();
        let audit = self.audit(&path)?;
        if audit.consistent {
            return Ok(Reconcile {
                rebuilt: false,
                issues: Vec::new(),
                warnings: Vec::new(),
            });
        }
        let warnings = self.rebuild_forced(std::slice::from_ref(&path))?;
        Ok(Reconcile {
            rebuilt: warnings.is_empty(),
            issues: audit.issues,
            warnings,
        })
    }
}

/// Replaces one file's index rows from a fresh scan. The caller owns the transaction.
/// The `seq` of every `MessageAppended` that is part of the committed history (ADR 0019).
/// Derived from the event names alone, so the streaming index does not retain messages.
fn committed_message_seqs(rows: &[IndexedEvent]) -> std::collections::HashSet<u64> {
    let mut committed = std::collections::HashSet::new();
    let mut pending: Option<Vec<u64>> = None;
    for row in rows {
        match row.event.as_str() {
            "MessageAppended" => match &mut pending {
                Some(pending) => pending.push(row.seq),
                None => {
                    committed.insert(row.seq);
                }
            },
            "TranscriptReplaced" => pending = Some(Vec::new()),
            "TranscriptReplacementCommitted" => {
                if let Some(pending) = pending.take() {
                    committed.extend(pending);
                }
            }
            "TranscriptReplacementAborted" => pending = None,
            _ => {}
        }
    }
    committed
}

/// The file's modification time in nanoseconds since the Unix epoch (0 when unavailable).
fn mtime_nanos(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn index_file(tx: &Connection, path: &Path) -> Result<(), IndexError> {
    let path_str = path.to_string_lossy().to_string();
    // Drop any previous rows for this path before re-deriving them.
    let old: Option<String> = tx
        .query_row(
            "SELECT session_id FROM sessions WHERE path = ?1",
            params![path_str],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(old) = &old {
        tx.execute("DELETE FROM events WHERE session_id = ?1", params![old])?;
        tx.execute("DELETE FROM sessions WHERE session_id = ?1", params![old])?;
    }

    // Collect the derivable columns in one streaming pass (no per-row file access, and the
    // session file is never held whole in memory).
    let mut rows: Vec<IndexedEvent> = Vec::new();
    let mut workspace: Option<String> = None;
    let mut created_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let summary = scan_stream::<IndexError, _>(path, |event| {
        if event.event == "SessionCreated" {
            created_ts = Some(event.ts.clone());
            if let EventData::Other(data) = &event.data {
                workspace = data
                    .get("workspace")
                    .and_then(|w| w.as_str())
                    .map(str::to_owned);
            }
        }
        last_ts = Some(event.ts.clone());
        rows.push(IndexedEvent {
            seq: event.seq,
            ts: event.ts,
            event: event.event,
            critical: event.critical,
            byte_offset: event.byte_offset,
            line_len: event.line_len,
            message_index: message_index(&event.data),
            text: visible_text(&event.data),
        });
        Ok(())
    })?;
    if summary.session_id.is_empty() {
        return Err(IndexError::Empty {
            path: path.to_path_buf(),
            message: "the file has no complete session line; nothing to index".into(),
        });
    }

    // ADR 0019: a message inside an uncommitted replacement segment is not in the current
    // history, so it is not indexed (and therefore not searchable).
    let committed = committed_message_seqs(&rows);
    let mut insert = tx.prepare(
        "INSERT INTO events \
         (session_id, seq, ts, event, critical, byte_offset, line_len, message_index, text) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for row in rows
        .iter()
        .filter(|row| row.event != "MessageAppended" || committed.contains(&row.seq))
    {
        insert.execute(params![
            summary.session_id,
            row.seq as i64,
            row.ts,
            row.event,
            row.critical as i64,
            row.byte_offset as i64,
            row.line_len as i64,
            row.message_index.map(|n| n as i64),
            row.text,
        ])?;
    }
    drop(insert);

    let skipped = summary.skipped.len() as u64;
    let meta = std::fs::metadata(path)?;
    tx.execute(
        "INSERT INTO sessions \
         (session_id, path, schema_version, workspace, created_ts, last_ts, event_count, \
          skipped_count, file_bytes, indexed_at, file_size, file_mtime) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            summary.session_id,
            path_str,
            summary.schema_version as i64,
            workspace,
            created_ts,
            last_ts,
            rows.len() as i64,
            skipped as i64,
            summary.committed_bytes as i64,
            now_utc(),
            meta.len() as i64,
            mtime_nanos(&meta),
        ],
    )?;
    Ok(())
}

/// The `data.index` of a `MessageAppended`, for searchable rows.
fn message_index(data: &EventData) -> Option<u64> {
    match data {
        EventData::MessageAppended { index, .. } => Some(*index),
        _ => None,
    }
}

/// The searchable visible text: the `Text` content parts of user and assistant messages.
/// Tool results, tool-call arguments and provider payloads are deliberately not indexed.
fn visible_text(data: &EventData) -> Option<String> {
    let EventData::MessageAppended { message, .. } = data else {
        return None;
    };
    let content = match message {
        Message::User { content, .. } | Message::Assistant { content, .. } => content,
        Message::Tool { .. } => return None,
    };
    let text = content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() { None } else { Some(text) }
}

/// Escapes `LIKE` metacharacters so a query is matched literally.
fn escape_like(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A short window around the first match, for display. Not a fact of the session.
fn snippet(text: &str, query: &str) -> String {
    const WINDOW: usize = 120;
    let lower = text.to_lowercase();
    let needle = query.to_lowercase();
    let start = lower.find(&needle).unwrap_or(0);
    let from = text[..start]
        .char_indices()
        .rev()
        .nth(WINDOW)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let to = text[start..]
        .char_indices()
        .nth(WINDOW)
        .map(|(i, _)| start + i)
        .unwrap_or(text.len());
    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.push_str(&text[from..to]);
    if to < text.len() {
        out.push('…');
    }
    out
}

fn now_utc() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

/// Reads the original line an index row points at: exactly `len` bytes at `offset`, with
/// the trailing LF removed. This is how a search hit returns to the authority.
pub fn read_line_at(path: impl AsRef<Path>, offset: u64, len: u64) -> Result<String, IndexError> {
    let mut file = std::fs::File::open(path.as_ref())?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact(&mut buf)?;
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
