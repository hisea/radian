//! The file store (ADR 0021 D3/D4): one Markdown record per file and one `MEMORY.md` index per
//! scope, beside the old JSONL logs.
//!
//! A record file is YAML frontmatter between two `---` lines and then the body. The frontmatter
//! is flat except for the optional `provenance` map, and every scalar is on one line: plain, or
//! single- or double-quoted. The parser is deliberately small and has no YAML dependency: a known
//! field with a known shape is accepted, anything else is refused with its 1-based line.
//!
//! The name is the file stem and is restricted to `[a-z0-9-]`, 1 to 64 characters. Two records
//! never share a name. A write goes through a temporary file in the scope directory and an
//! atomic rename, like the registry; a crash leaves either the old file or the new one.
//!
//! `migrate_log` converts one scope's old `log.jsonl` once, with the old reader, then renames
//! the log to `log.jsonl.migrated`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::DateTime;

use crate::error::MemoryError;
use crate::event::MemoryEvent;
use crate::fs::{create_private_file, ensure_private_dir};
use crate::log::Log;
use crate::model::{Kind, MemoryRecord, Scope, Source, now_utc};

/// The closed `type` set (ADR 0021 D4). The parser, the writer and the docs drift test read it.
pub const RECORD_TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// The closed `source` set (ADR 0021 D4). The host sets the value; the model never does.
pub const RECORD_SOURCES: &[&str] = &["model", "user", "import"];

/// The longest record name, in characters.
pub const NAME_MAX: usize = 64;

/// The longest index description, in characters.
pub const DESCRIPTION_MAX: usize = 120;

const INDEX_FILE: &str = "MEMORY.md";
/// The host-maintained session index file of a project scope (ADR 0021 D3).
pub const SESSIONS_INDEX_FILE: &str = "SESSIONS.md";
const LOG_FILE: &str = "log.jsonl";
const MIGRATED_LOG_FILE: &str = "log.jsonl.migrated";

/// The optional `provenance` map of a record. Both keys are optional and there are no others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileProvenance {
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

/// The frontmatter of one record file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub name: String,
    pub description: String,
    /// The `type` field; one of [`RECORD_TYPES`].
    pub record_type: String,
    /// The `source` field; one of [`RECORD_SOURCES`].
    pub source: String,
    /// RFC 3339 UTC.
    pub created: String,
    /// RFC 3339 UTC.
    pub modified: String,
    /// RFC 3339 UTC, or `None` for a record that was never read.
    pub last_used: Option<String>,
    pub provenance: Option<FileProvenance>,
}

/// One parsed record: its frontmatter and its body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// What a host supplies to [`FileStore::write`]. `modified` is set by the store; `created` and
/// `last_used` are kept from an existing file, and so is `source` after the first write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecord {
    pub name: String,
    pub description: String,
    /// One of [`RECORD_TYPES`].
    pub record_type: String,
    /// One of [`RECORD_SOURCES`].
    pub source: String,
    pub provenance: Option<FileProvenance>,
    pub body: String,
}

/// One entry [`FileStore::list`] returns: a readable record or one file it could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListEntry {
    Record(Frontmatter),
    /// A file whose frontmatter does not parse, whose `type` or `source` is outside its closed
    /// set, or whose `name` is not its file stem. `reason` names the line.
    Unreadable {
        path: PathBuf,
        reason: String,
    },
}

/// One line of an index as [`index_lines`] reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexLine {
    Entry {
        /// 1-based line number.
        line: usize,
        title: String,
        file: String,
        description: String,
    },
    /// A line that is not in the `- [title](file) — description` form.
    Invalid { line: usize },
}

/// The file store, rooted at the memory directory (the same root the registry uses).
#[derive(Debug, Clone)]
pub struct FileStore {
    root: PathBuf,
}

impl FileStore {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/global` or `<root>/projects/<ProjectId>`.
    pub fn scope_dir(&self, scope: &Scope) -> PathBuf {
        match scope {
            Scope::Global => self.root.join("global"),
            Scope::Project(project) => self.root.join("projects").join(project.as_str()),
        }
    }

    pub fn record_path(&self, scope: &Scope, name: &str) -> PathBuf {
        self.scope_dir(scope).join(format!("{name}.md"))
    }

    pub fn index_path(&self, scope: &Scope) -> PathBuf {
        self.scope_dir(scope).join(INDEX_FILE)
    }

    /// `<scope>/SESSIONS.md`, the host-maintained session index (ADR 0021 D3).
    pub fn sessions_index_path(&self, scope: &Scope) -> PathBuf {
        self.scope_dir(scope).join(SESSIONS_INDEX_FILE)
    }

    /// Every readable record's frontmatter, sorted by `name`, followed by one `Unreadable`
    /// entry per file that does not parse. An absent scope directory lists as empty and is not
    /// created.
    pub fn list(&self, scope: &Scope) -> Result<Vec<ListEntry>, MemoryError> {
        list_dir(&self.scope_dir(scope))
    }

    /// One record's frontmatter and body. A broken file is an error naming the path and line.
    pub fn read(&self, scope: &Scope, name: &str) -> Result<Record, MemoryError> {
        read_named(&self.scope_dir(scope), name)
    }

    /// Creates or replaces `<name>.md`. `modified` is set now; `created`, `last_used` and
    /// `source` are kept from an existing file. The name is validated before any file is touched.
    pub fn write(&self, scope: &Scope, record: NewRecord) -> Result<Record, MemoryError> {
        write_new(&self.scope_dir(scope), record)
    }

    /// Deletes `<name>.md`: deleting a file is forgetting.
    pub fn delete(&self, scope: &Scope, name: &str) -> Result<(), MemoryError> {
        delete_named(&self.scope_dir(scope), name)
    }

    /// Sets `last_used` to `at`, rewriting only that line. Every other byte is unchanged.
    pub fn touch_last_used(&self, scope: &Scope, name: &str, at: &str) -> Result<(), MemoryError> {
        touch_last_used(&self.scope_dir(scope), name, at)
    }

    /// The index as written, or an empty string when the scope has none.
    pub fn read_index(&self, scope: &Scope) -> Result<String, MemoryError> {
        read_index_dir(&self.scope_dir(scope))
    }

    /// Replaces the index with `text`, verbatim. The store does not interpret it.
    pub fn write_index(&self, scope: &Scope, text: &str) -> Result<(), MemoryError> {
        write_index_dir(&self.scope_dir(scope), text)
    }

    /// The session index as written, or an empty string when the scope has none.
    pub fn read_sessions_index(&self, scope: &Scope) -> Result<String, MemoryError> {
        read_plain_file(&self.sessions_index_path(scope))
    }

    /// Replaces the session index with `text` through a temporary file and an atomic rename
    /// (ADR 0021 D3). The store does not interpret it.
    pub fn write_sessions_index(&self, scope: &Scope, text: &str) -> Result<(), MemoryError> {
        atomic_write(&self.sessions_index_path(scope), text.as_bytes())
    }
}

fn record_path_in(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.md"))
}

fn validate_name(name: &str) -> Result<(), MemoryError> {
    let reason = if name.is_empty() {
        "the name is empty".to_owned()
    } else if name.chars().count() > NAME_MAX {
        format!("the name is longer than {NAME_MAX} characters")
    } else if !name
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        "a name may contain only a-z, 0-9 and -".to_owned()
    } else {
        return Ok(());
    };
    Err(MemoryError::InvalidName {
        name: name.to_owned(),
        reason,
    })
}

fn validate_timestamp(field: &str, value: &str) -> Result<(), String> {
    let parsed = DateTime::parse_from_rfc3339(value)
        .map_err(|error| format!("{field} {value:?} is not RFC 3339: {error}"))?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(format!("{field} {value:?} is not UTC"));
    }
    Ok(())
}

fn validate_frontmatter(frontmatter: &Frontmatter) -> Result<(), MemoryError> {
    validate_name(&frontmatter.name)?;
    if frontmatter.description.contains(['\n', '\r']) {
        return Err(invalid("the description must be one line"));
    }
    if !RECORD_TYPES.contains(&frontmatter.record_type.as_str()) {
        return Err(invalid(format!(
            "type {:?} is not one of {:?}",
            frontmatter.record_type, RECORD_TYPES
        )));
    }
    if !RECORD_SOURCES.contains(&frontmatter.source.as_str()) {
        return Err(invalid(format!(
            "source {:?} is not one of {:?}",
            frontmatter.source, RECORD_SOURCES
        )));
    }
    validate_timestamp("created", &frontmatter.created).map_err(invalid)?;
    validate_timestamp("modified", &frontmatter.modified).map_err(invalid)?;
    if let Some(last_used) = &frontmatter.last_used {
        validate_timestamp("last_used", last_used).map_err(invalid)?;
    }
    if let Some(provenance) = &frontmatter.provenance {
        for (field, value) in [
            ("session_id", provenance.session_id.as_deref()),
            ("turn_id", provenance.turn_id.as_deref()),
        ] {
            if let Some(value) = value
                && value.contains(['\n', '\r'])
            {
                return Err(invalid(format!("provenance {field} must be one line")));
            }
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> MemoryError {
    MemoryError::InvalidRecord {
        message: message.into(),
    }
}

/// One parsed line: its text without the newline and the remaining bytes.
fn take_line(text: &str) -> (&str, &str) {
    match text.find('\n') {
        Some(index) => (&text[..index], &text[index + 1..]),
        None => (text, ""),
    }
}

/// One frontmatter line: its byte range and its text without the `\r` of a `\r\n` ending.
struct Line<'a> {
    start: usize,
    content_end: usize,
    next: usize,
    text: &'a str,
}

fn line_at(text: &str, cursor: usize) -> Line<'_> {
    let (raw, rest) = take_line(&text[cursor..]);
    Line {
        start: cursor,
        content_end: cursor + raw.len(),
        next: text.len() - rest.len(),
        text: raw.strip_suffix('\r').unwrap_or(raw),
    }
}

#[derive(Debug)]
struct ParseError {
    line: usize,
    message: String,
}

impl ParseError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

/// Parses a YAML scalar as this store accepts it: a plain scalar, or a single- or double-quoted
/// one on a single line. A plain scalar ends at a ` #` comment and loses trailing whitespace.
fn parse_scalar(value: &str, line: usize) -> Result<String, ParseError> {
    let value = value.trim_start();
    if let Some(rest) = value.strip_prefix('"') {
        parse_double_quoted(rest, line)
    } else if let Some(rest) = value.strip_prefix('\'') {
        parse_single_quoted(rest, line)
    } else {
        Ok(plain_scalar(value))
    }
}

fn plain_scalar(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut end = bytes.len();
    for index in 1..bytes.len() {
        if bytes[index] == b'#' && matches!(bytes[index - 1], b' ' | b'\t') {
            end = index;
            break;
        }
    }
    value[..end].trim_end().to_owned()
}

fn parse_double_quoted(rest: &str, line: usize) -> Result<String, ParseError> {
    let mut chars = rest.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if !scalar_tail_ok(chars.as_str()) {
                    return Err(ParseError::new(
                        line,
                        "unexpected text after a quoted scalar",
                    ));
                }
                return Ok(out);
            }
            '\\' => {
                let Some(escaped) = chars.next() else {
                    return Err(ParseError::new(line, "a quoted scalar ends in a backslash"));
                };
                match escaped {
                    '0' => out.push('\0'),
                    'a' => out.push('\u{7}'),
                    'b' => out.push('\u{8}'),
                    't' | '\t' => out.push('\t'),
                    'n' => out.push('\n'),
                    'v' => out.push('\u{b}'),
                    'f' => out.push('\u{c}'),
                    'r' => out.push('\r'),
                    'e' => out.push('\u{1b}'),
                    ' ' => out.push(' '),
                    '"' => out.push('"'),
                    '/' => out.push('/'),
                    '\\' => out.push('\\'),
                    'N' => out.push('\u{85}'),
                    '_' => out.push('\u{a0}'),
                    'L' => out.push('\u{2028}'),
                    'P' => out.push('\u{2029}'),
                    'x' => out.push(parse_hex_escape(&mut chars, 2, line)?),
                    'u' => out.push(parse_hex_escape(&mut chars, 4, line)?),
                    'U' => out.push(parse_hex_escape(&mut chars, 8, line)?),
                    other => {
                        return Err(ParseError::new(
                            line,
                            format!("unknown escape `\\{other}` in a quoted scalar"),
                        ));
                    }
                }
            }
            _ => out.push(ch),
        }
    }
    Err(ParseError::new(line, "a quoted scalar is not closed"))
}

fn parse_hex_escape(
    chars: &mut std::str::Chars<'_>,
    digits: usize,
    line: usize,
) -> Result<char, ParseError> {
    let mut value = 0u32;
    for _ in 0..digits {
        let Some(ch) = chars.next() else {
            return Err(ParseError::new(line, "a quoted scalar ends mid-escape"));
        };
        let Some(digit) = ch.to_digit(16) else {
            return Err(ParseError::new(
                line,
                format!("`{ch}` is not a hex digit in a quoted scalar"),
            ));
        };
        value = value * 16 + digit;
    }
    char::from_u32(value)
        .ok_or_else(|| ParseError::new(line, "a quoted scalar has an invalid escape code point"))
}

fn parse_single_quoted(rest: &str, line: usize) -> Result<String, ParseError> {
    let mut chars = rest.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.as_str().starts_with('\'') {
                chars.next();
                out.push('\'');
            } else {
                if !scalar_tail_ok(chars.as_str()) {
                    return Err(ParseError::new(
                        line,
                        "unexpected text after a quoted scalar",
                    ));
                }
                return Ok(out);
            }
        } else {
            out.push(ch);
        }
    }
    Err(ParseError::new(line, "a quoted scalar is not closed"))
}

/// A quoted scalar may be followed only by whitespace and a `#` comment on its line.
fn scalar_tail_ok(tail: &str) -> bool {
    let tail = tail.trim_start();
    tail.is_empty() || tail.starts_with('#')
}

#[derive(Default)]
struct Builder {
    name: Option<String>,
    description: Option<String>,
    record_type: Option<String>,
    source: Option<String>,
    created: Option<String>,
    modified: Option<String>,
    last_used: Option<String>,
    provenance: Option<FileProvenance>,
}

impl Builder {
    fn finish(self) -> Result<Frontmatter, ParseError> {
        let required = |value: Option<String>, field: &str| {
            value.ok_or_else(|| ParseError::new(1, format!("frontmatter is missing {field}")))
        };
        Ok(Frontmatter {
            name: required(self.name, "name")?,
            description: required(self.description, "description")?,
            record_type: required(self.record_type, "type")?,
            source: required(self.source, "source")?,
            created: required(self.created, "created")?,
            modified: required(self.modified, "modified")?,
            last_used: self.last_used,
            provenance: self.provenance,
        })
    }
}

/// Parses one record file. `expected_name`, when given, must equal both the `name` field and
/// the file stem the caller read the text from.
fn parse_record(text: &str, expected_name: Option<&str>) -> Result<Record, ParseError> {
    let first = line_at(text, 0);
    if first.text.trim() != "---" {
        return Err(ParseError::new(1, "the frontmatter must start with `---`"));
    }
    let mut cursor = first.next;
    let mut line_number = 1usize;
    let mut builder = Builder::default();
    let mut in_provenance = false;
    let body_start = loop {
        if cursor >= text.len() {
            return Err(ParseError::new(
                1,
                "the frontmatter is not closed with `---`",
            ));
        }
        let line = line_at(text, cursor);
        line_number += 1;
        let trimmed = line.text.trim();
        if trimmed == "---" {
            break line.next;
        }
        if trimmed.is_empty() {
            cursor = line.next;
            continue;
        }
        if line.text.starts_with(' ') || line.text.starts_with('\t') {
            if !in_provenance {
                return Err(ParseError::new(
                    line_number,
                    "unexpected indented line outside `provenance`",
                ));
            }
            let Some((key, value)) = line.text.trim().split_once(':') else {
                return Err(ParseError::new(line_number, "expected `key: value`"));
            };
            let value = parse_scalar(value, line_number)?;
            let provenance = builder.provenance.get_or_insert_with(Default::default);
            match key.trim() {
                "session_id" => provenance.session_id = Some(value),
                "turn_id" => provenance.turn_id = Some(value),
                // An unknown key under `provenance` is ignored, like an unknown top-level key.
                _ => {}
            }
            cursor = line.next;
            continue;
        }
        in_provenance = false;
        let Some((key, value)) = line.text.split_once(':') else {
            return Err(ParseError::new(line_number, "expected `key: value`"));
        };
        let key = key.trim();
        match key {
            "name" => {
                let value = parse_scalar(value, line_number)?;
                if !name_fits(&value) {
                    return Err(ParseError::new(
                        line_number,
                        format!(
                            "name {value:?} must match [a-z0-9-] and be 1 to {NAME_MAX} characters"
                        ),
                    ));
                }
                builder.name = Some(value);
            }
            "description" => {
                let value = parse_scalar(value, line_number)?;
                if value.contains(['\n', '\r']) {
                    return Err(ParseError::new(
                        line_number,
                        "the description must be one line",
                    ));
                }
                builder.description = Some(value);
            }
            "type" => {
                let value = parse_scalar(value, line_number)?;
                if !RECORD_TYPES.contains(&value.as_str()) {
                    return Err(ParseError::new(
                        line_number,
                        format!("type {value:?} is not one of {RECORD_TYPES:?}"),
                    ));
                }
                builder.record_type = Some(value);
            }
            "source" => {
                let value = parse_scalar(value, line_number)?;
                if !RECORD_SOURCES.contains(&value.as_str()) {
                    return Err(ParseError::new(
                        line_number,
                        format!("source {value:?} is not one of {RECORD_SOURCES:?}"),
                    ));
                }
                builder.source = Some(value);
            }
            "created" => {
                let value = parse_scalar(value, line_number)?;
                validate_timestamp("created", &value)
                    .map_err(|message| ParseError::new(line_number, message))?;
                builder.created = Some(value);
            }
            "modified" => {
                let value = parse_scalar(value, line_number)?;
                validate_timestamp("modified", &value)
                    .map_err(|message| ParseError::new(line_number, message))?;
                builder.modified = Some(value);
            }
            "last_used" => {
                let value = parse_scalar(value, line_number)?;
                if value.is_empty() || value == "null" || value == "~" {
                    builder.last_used = None;
                } else {
                    validate_timestamp("last_used", &value)
                        .map_err(|message| ParseError::new(line_number, message))?;
                    builder.last_used = Some(value);
                }
            }
            "provenance" => match value.trim() {
                "" => {
                    builder.provenance = Some(FileProvenance::default());
                    in_provenance = true;
                }
                "{}" | "null" | "~" => builder.provenance = Some(FileProvenance::default()),
                other => {
                    return Err(ParseError::new(
                        line_number,
                        format!("provenance must be a map, not {other:?}"),
                    ));
                }
            },
            // An unknown top-level key is forward compatible and ignored.
            _ => {}
        }
        cursor = line.next;
    };
    let frontmatter = builder.finish()?;
    if let Some(expected) = expected_name
        && frontmatter.name != expected
    {
        return Err(ParseError::new(
            1,
            format!(
                "name {:?} does not match the file stem {expected:?}",
                frontmatter.name
            ),
        ));
    }
    Ok(Record {
        frontmatter,
        body: text[body_start..].to_owned(),
    })
}

fn name_fits(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_MAX
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

fn frontmatter_error(path: PathBuf, error: ParseError) -> MemoryError {
    MemoryError::Frontmatter {
        path,
        line: error.line,
        message: error.message,
    }
}

fn read_named(dir: &Path, name: &str) -> Result<Record, MemoryError> {
    validate_name(name)?;
    let path = record_path_in(dir, name);
    let text = std::fs::read_to_string(&path).map_err(|error| MemoryError::io(&path, error))?;
    parse_record(&text, Some(name)).map_err(|error| frontmatter_error(path, error))
}

fn list_dir(dir: &Path) -> Result<Vec<ListEntry>, MemoryError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(MemoryError::io(dir, error)),
    };
    let mut paths: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| MemoryError::io(dir, error))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name.starts_with('.') || file_name == INDEX_FILE || file_name == SESSIONS_INDEX_FILE
        {
            continue;
        }
        let Some(stem) = file_name.strip_suffix(".md") else {
            continue;
        };
        paths.push((stem.to_owned(), path));
    }
    paths.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let mut readable = Vec::new();
    let mut unreadable = Vec::new();
    for (stem, path) in paths {
        match std::fs::read_to_string(&path) {
            Ok(text) => match parse_record(&text, Some(&stem)) {
                Ok(record) => readable.push(ListEntry::Record(record.frontmatter)),
                Err(error) => unreadable.push(ListEntry::Unreadable {
                    path,
                    reason: format!("line {}: {}", error.line, error.message),
                }),
            },
            Err(error) => unreadable.push(ListEntry::Unreadable {
                path,
                reason: error.to_string(),
            }),
        }
    }
    // The records were collected in stem order and `name` must equal the stem, so they are
    // already in name order. Unreadable files carry no trustworthy name and follow.
    readable.extend(unreadable);
    Ok(readable)
}

fn write_new(dir: &Path, new: NewRecord) -> Result<Record, MemoryError> {
    validate_name(&new.name)?;
    if new.description.contains(['\n', '\r']) {
        return Err(invalid("the description must be one line"));
    }
    if !RECORD_TYPES.contains(&new.record_type.as_str()) {
        return Err(invalid(format!(
            "type {:?} is not one of {RECORD_TYPES:?}",
            new.record_type
        )));
    }
    if !RECORD_SOURCES.contains(&new.source.as_str()) {
        return Err(invalid(format!(
            "source {:?} is not one of {RECORD_SOURCES:?}",
            new.source
        )));
    }
    let path = record_path_in(dir, &new.name);
    // `created` and `last_used` are kept across a replacement, and so is `source`: a record's
    // source is fixed at its first write (ADR 0021 D4).
    let (created, last_used, source) = if path.exists() {
        let existing = read_named(dir, &new.name)?;
        (
            existing.frontmatter.created,
            existing.frontmatter.last_used,
            existing.frontmatter.source,
        )
    } else {
        (now_utc(), None, new.source)
    };
    let frontmatter = Frontmatter {
        name: new.name.clone(),
        description: new.description,
        record_type: new.record_type,
        source,
        created,
        modified: now_utc(),
        last_used,
        provenance: new.provenance,
    };
    write_record(dir, &frontmatter, &new.body)?;
    Ok(Record {
        frontmatter,
        body: new.body,
    })
}

fn write_record(dir: &Path, frontmatter: &Frontmatter, body: &str) -> Result<(), MemoryError> {
    validate_frontmatter(frontmatter)?;
    atomic_write(
        &record_path_in(dir, &frontmatter.name),
        render(frontmatter, body).as_bytes(),
    )
}

fn render(frontmatter: &Frontmatter, body: &str) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", render_scalar(&frontmatter.name)));
    out.push_str(&format!(
        "description: {}\n",
        render_scalar(&frontmatter.description)
    ));
    out.push_str(&format!(
        "type: {}\n",
        render_scalar(&frontmatter.record_type)
    ));
    out.push_str(&format!("source: {}\n", render_scalar(&frontmatter.source)));
    out.push_str(&format!(
        "created: {}\n",
        render_scalar(&frontmatter.created)
    ));
    out.push_str(&format!(
        "modified: {}\n",
        render_scalar(&frontmatter.modified)
    ));
    if let Some(last_used) = &frontmatter.last_used {
        out.push_str(&format!("last_used: {}\n", render_scalar(last_used)));
    }
    if let Some(provenance) = &frontmatter.provenance
        && (provenance.session_id.is_some() || provenance.turn_id.is_some())
    {
        out.push_str("provenance:\n");
        if let Some(session_id) = &provenance.session_id {
            out.push_str(&format!("  session_id: {}\n", render_scalar(session_id)));
        }
        if let Some(turn_id) = &provenance.turn_id {
            out.push_str(&format!("  turn_id: {}\n", render_scalar(turn_id)));
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

fn render_scalar(value: &str) -> String {
    if plain_scalar_safe(value) {
        value.to_owned()
    } else {
        quote_scalar(value)
    }
}

/// A value the writer can emit without quotes and read back unchanged.
fn plain_scalar_safe(value: &str) -> bool {
    if value.is_empty() || value != value.trim() || value.chars().any(char::is_control) {
        return false;
    }
    let first = value.chars().next().unwrap();
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return false;
    }
    if value.contains(": ") || value.ends_with(':') || value.contains(" #") {
        return false;
    }
    !matches!(
        value,
        "null" | "Null" | "NULL" | "~" | "true" | "True" | "TRUE" | "false" | "False" | "FALSE"
    )
}

fn quote_scalar(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn delete_named(dir: &Path, name: &str) -> Result<(), MemoryError> {
    validate_name(name)?;
    let path = record_path_in(dir, name);
    std::fs::remove_file(&path).map_err(|error| MemoryError::io(&path, error))?;
    fsync_dir(dir)
}

fn touch_last_used(dir: &Path, name: &str, at: &str) -> Result<(), MemoryError> {
    validate_name(name)?;
    validate_timestamp("last_used", at).map_err(invalid)?;
    let path = record_path_in(dir, name);
    let text = std::fs::read_to_string(&path).map_err(|error| MemoryError::io(&path, error))?;
    // Refuse a broken file rather than rewriting part of it.
    parse_record(&text, Some(name)).map_err(|error| frontmatter_error(path.clone(), error))?;
    let updated =
        replace_last_used(&text, at).map_err(|error| frontmatter_error(path.clone(), error))?;
    if updated != text {
        atomic_write(&path, updated.as_bytes())?;
    }
    Ok(())
}

/// Replaces the `last_used` line, or inserts one after `modified`, and leaves every other byte
/// alone.
fn replace_last_used(text: &str, at: &str) -> Result<String, ParseError> {
    let first = line_at(text, 0);
    if first.text.trim() != "---" {
        return Err(ParseError::new(1, "the frontmatter must start with `---`"));
    }
    let mut cursor = first.next;
    let mut line_number = 1usize;
    let mut modified_end: Option<usize> = None;
    let mut closing_start: Option<usize> = None;
    let mut existing: Option<(usize, usize, usize)> = None;
    loop {
        if cursor >= text.len() {
            return Err(ParseError::new(
                1,
                "the frontmatter is not closed with `---`",
            ));
        }
        let line = line_at(text, cursor);
        line_number += 1;
        let trimmed = line.text.trim();
        if trimmed == "---" {
            closing_start = Some(line.start);
            break;
        }
        let indented = line.text.starts_with(' ') || line.text.starts_with('\t');
        if !indented {
            if line.text.starts_with("last_used:") {
                existing = Some((line.start, line.content_end, line.next));
                break;
            }
            if line.text.starts_with("modified:") {
                modified_end = Some(line.next);
            }
        }
        cursor = line.next;
    }
    match existing {
        Some((start, content_end, next)) => {
            let ending = &text[content_end..next];
            let mut replacement = format!("last_used: {}", render_scalar(at));
            replacement.push_str(ending);
            Ok(format!(
                "{}{}{}",
                &text[..start],
                replacement,
                &text[next..]
            ))
        }
        None => {
            let insert_at = modified_end.or(closing_start).ok_or_else(|| {
                ParseError::new(line_number, "the frontmatter is not closed with `---`")
            })?;
            Ok(format!(
                "{}last_used: {}\n{}",
                &text[..insert_at],
                render_scalar(at),
                &text[insert_at..]
            ))
        }
    }
}

fn read_index_dir(dir: &Path) -> Result<String, MemoryError> {
    read_plain_file(&dir.join(INDEX_FILE))
}

fn read_plain_file(path: &Path) -> Result<String, MemoryError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(MemoryError::io(path, error)),
    }
}

fn write_index_dir(dir: &Path, text: &str) -> Result<(), MemoryError> {
    atomic_write(&dir.join(INDEX_FILE), text.as_bytes())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), MemoryError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_dir(dir).map_err(|error| MemoryError::io(dir, error))?;
    let temp = temp_path(path);
    {
        let mut file = create_private_file(&temp).map_err(|error| MemoryError::io(&temp, error))?;
        file.write_all(bytes)
            .map_err(|error| MemoryError::io(&temp, error))?;
        file.sync_all()
            .map_err(|error| MemoryError::io(&temp, error))?;
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(MemoryError::io(path, error));
    }
    fsync_dir(dir)
}

fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "memory".into());
    path.with_file_name(format!(".{name}.tmp-{}-{n}", std::process::id()))
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<(), MemoryError> {
    let handle = std::fs::File::open(dir).map_err(|error| MemoryError::io(dir, error))?;
    handle
        .sync_all()
        .map_err(|error| MemoryError::io(dir, error))
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<(), MemoryError> {
    Ok(())
}

/// Parses the `- [title](file) — description` index form. A line that is not in it is
/// [`IndexLine::Invalid`] with its 1-based number; the other lines still parse.
pub fn index_lines(text: &str) -> Vec<IndexLine> {
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            let line_number = index + 1;
            match parse_index_line(line) {
                Some((title, file, description)) => IndexLine::Entry {
                    line: line_number,
                    title,
                    file,
                    description,
                },
                None => IndexLine::Invalid { line: line_number },
            }
        })
        .collect()
}

fn parse_index_line(line: &str) -> Option<(String, String, String)> {
    let rest = line.strip_prefix("- [")?;
    let close = rest.find("](")?;
    let title = &rest[..close];
    let rest = &rest[close + 2..];
    let paren = rest.find(')')?;
    let file = &rest[..paren];
    let description = rest[paren + 1..].strip_prefix(" — ")?;
    if title.is_empty() || file.is_empty() {
        return None;
    }
    Some((title.to_owned(), file.to_owned(), description.to_owned()))
}

/// Converts one scope's old `log.jsonl` into files and an index, once. A missing log or an
/// existing `log.jsonl.migrated` is a no-op; a crash before the rename leaves the log, and the
/// next call writes the same files again (the writes are idempotent).
pub fn migrate_log(scope_dir: impl AsRef<Path>) -> Result<(), MemoryError> {
    let dir = scope_dir.as_ref();
    let log_path = dir.join(LOG_FILE);
    let migrated = dir.join(MIGRATED_LOG_FILE);
    if migrated.exists() || !log_path.exists() {
        return Ok(());
    }

    // Replay with today's reader. The `Log` (and its writer lock) is dropped before any file
    // write; the events are the whole input.
    let Some((_, events)) = Log::open_existing(&log_path, MemoryEvent::NAMES)? else {
        return Ok(());
    };
    let mut records: BTreeMap<crate::model::MemoryId, MemoryRecord> = BTreeMap::new();
    let mut forgotten = BTreeSet::new();
    for accepted in &events {
        let event = MemoryEvent::from_data(&accepted.event, &accepted.data).map_err(|error| {
            MemoryError::Corrupt {
                path: log_path.clone(),
                line: accepted.line,
                column: 0,
                message: error.to_string(),
            }
        })?;
        event
            .apply(&mut records, &mut forgotten)
            .map_err(|message| MemoryError::Corrupt {
                path: log_path.clone(),
                line: accepted.line,
                column: 0,
                message,
            })?;
    }

    let mut taken = BTreeSet::new();
    let mut index = String::new();
    for record in records.values() {
        if !record.is_active() {
            continue;
        }
        let name = unique_name(&base_name(record), &mut taken);
        let description = description_of(&record.content);
        let frontmatter = Frontmatter {
            name: name.clone(),
            description: description.clone(),
            record_type: type_for_kind(&record.kind).to_owned(),
            source: source_for_source(&record.source),
            created: record.created_at.clone(),
            modified: record.updated_at.clone(),
            last_used: None,
            provenance: provenance_of(record),
        };
        write_record(dir, &frontmatter, &record.content)?;
        index.push_str(&format!("- [{name}]({name}.md) — {description}\n"));
    }
    write_index_dir(dir, &index)?;

    if crate::store::run_move_hook(crate::store::MovePhase::MigrationBeforeLogRename) {
        return Err(MemoryError::SimulatedCrash);
    }
    std::fs::rename(&log_path, &migrated).map_err(|error| MemoryError::io(&migrated, error))?;
    fsync_dir(dir)
}

fn base_name(record: &MemoryRecord) -> String {
    if let Some(key) = &record.key
        && name_fits(key)
    {
        return key.clone();
    }
    slug_of(&record.content)
}

fn description_of(content: &str) -> String {
    content
        .split(['\n', '\r'])
        .next()
        .unwrap_or("")
        .chars()
        .take(DESCRIPTION_MAX)
        .collect()
}

fn slug_of(content: &str) -> String {
    let mut slug = String::new();
    for word in content.split_whitespace().take(8) {
        if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        for ch in word.chars() {
            let lower = ch.to_ascii_lowercase();
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() {
                slug.push(lower);
            } else if !slug.ends_with('-') {
                slug.push('-');
            }
        }
    }
    let mut slug = slug.trim_matches('-').to_owned();
    if slug.chars().count() > NAME_MAX {
        slug = slug.chars().take(NAME_MAX).collect();
        slug = slug.trim_end_matches('-').to_owned();
    }
    if slug.is_empty() {
        slug.push_str("memory");
    }
    slug
}

fn unique_name(base: &str, taken: &mut BTreeSet<String>) -> String {
    let base = if base.is_empty() { "memory" } else { base };
    if taken.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut n = 2u64;
    loop {
        let suffix = format!("-{n}");
        let max = NAME_MAX.saturating_sub(suffix.chars().count());
        let mut stem: String = base.chars().take(max).collect();
        while stem.ends_with('-') {
            stem.pop();
        }
        if stem.is_empty() {
            stem.push_str("memory");
        }
        let candidate = format!("{stem}{suffix}");
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

fn type_for_kind(kind: &Kind) -> &'static str {
    match kind {
        Kind::Preference => "user",
        // An unknown old kind has no new counterpart; `project` is the general factual bucket.
        Kind::Decision | Kind::Fact | Kind::Unknown(_) => "project",
        Kind::Lesson => "feedback",
    }
}

fn source_for_source(source: &Source) -> String {
    match source {
        Source::User => "user".to_owned(),
        Source::Agent => "model".to_owned(),
        Source::Import => "import".to_owned(),
        // A value a newer old writer defined is kept when it is already a new source.
        Source::Unknown(raw) if RECORD_SOURCES.contains(&raw.as_str()) => raw.clone(),
        Source::Unknown(_) => "import".to_owned(),
    }
}

fn provenance_of(record: &MemoryRecord) -> Option<FileProvenance> {
    let session_id = record.provenance.session_id.clone();
    let turn_id = record.provenance.turn_id.clone();
    if session_id.is_none() && turn_id.is_none() {
        return None;
    }
    Some(FileProvenance {
        session_id,
        turn_id,
    })
}
