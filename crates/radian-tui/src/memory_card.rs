//! What a tool card shows when the model touches a memory file (ADR 0021, ADR 0035).
//!
//! The transcript resolves a memory path when a tool call's result arrives, from the recorded
//! call and result alone: a `write` carries the whole file in its `content` argument and a
//! `read` returns the file's numbered lines. The file itself is never read, so a resumed
//! transcript draws the same card a live session drew, even after the record changed or
//! disappeared (ADR 0035).
//!
//! A readable record's frontmatter `name` is its file stem (ADR 0021 D3), so the title is the
//! stem and a file whose frontmatter does not parse falls back to the same value. Only the
//! `type` needs the frontmatter. [`frontmatter_type`] mirrors `radian-memory`'s `parse_record`
//! rule for rule — the required fields, both closed sets, the name rule, the scalar syntax and
//! the timestamps — and the parity test below pins the two together.
//!
//! A `patch` names no content of its own. Its card carries the type the transcript recorded for
//! the same file earlier (a `write`'s content or a `read`'s result), so the value is a function
//! of the journaled messages alone; `?` is the type the transcript recorded none for. The path
//! rules mirror the store's (`radian-memory`'s `FileStore`): one `.md` file directly under
//! `global/` or `projects/<id>/`, never a deeper directory.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The closed `type` set (`radian-memory`'s `RECORD_TYPES`). Duplicated here because the TUI
/// does not depend on the memory crate at run time and cannot edit it; the parity test reads
/// both slices.
const RECORD_TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// The closed `source` set (`radian-memory`'s `RECORD_SOURCES`), pinned by the same test.
const RECORD_SOURCES: &[&str] = &["model", "user", "import"];

/// The longest record name (`radian-memory`'s `NAME_MAX`).
const NAME_MAX: usize = 64;

/// One memory file a successful call touched, as the transcript draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRef {
    /// The file stem (a readable record's frontmatter `name` is the stem).
    pub title: String,
    /// The frontmatter `type` the recorded calls establish; `None` when they establish none
    /// (the card shows `?`).
    pub record_type: Option<String>,
    /// The absolute file, for the opener.
    pub path: PathBuf,
    /// The path relative to the memory root, for the card (`projects/<id>/x.md`).
    pub relative: PathBuf,
    /// Whether this is a `read` (one line) rather than a `write`/`patch` (one card).
    pub read: bool,
}

impl MemoryRef {
    /// The type the card shows: the closed-set value, or `?` when the transcript recorded none.
    pub fn type_label(&self) -> &str {
        self.record_type.as_deref().unwrap_or("?")
    }

    /// The card's one body line: `<title> · <type> · <relative path>`.
    pub fn line(&self) -> String {
        format!(
            "{} · {} · {}",
            self.title,
            self.type_label(),
            self.relative.display()
        )
    }
}

/// The record state the transcript has seen, in message order (ADR 0035): the frontmatter
/// `type` of every file a `write` or `read` recorded. A `patch` card reads it, so the card is a
/// fold over the journaled messages and a resume redraws it exactly.
#[derive(Debug, Clone, Default)]
pub struct MemoryRecords {
    /// The last type one record file's frontmatter established. `None` is a file the recorded
    /// call showed to the store's parser as unreadable.
    types: HashMap<PathBuf, Option<String>>,
}

impl MemoryRecords {
    /// Resolves one settled tool call to a memory file, or `None` when its path is not a
    /// record.
    ///
    /// `root` is the memory root (the parent of `global/` and `projects/`); `tool` is the exact
    /// built-in name (`read`, `write` or `patch`); `arguments` is the call's JSON; `result` is
    /// the settled result's text. Nothing is read from disk.
    pub fn settle(
        &mut self,
        root: Option<&Path>,
        tool: &str,
        arguments: &Value,
        result: &str,
    ) -> Option<MemoryRef> {
        let root = root?;
        if !matches!(tool, "read" | "write" | "patch") {
            return None;
        }
        let raw = arguments.get("path").and_then(Value::as_str)?;
        let absolute = absolute_path(raw)?;
        let lexical_root = normalize(root);
        let root = canonical_root(root);
        let absolute = record_path(&absolute);
        // The canonical pair first; a record whose parent directory is gone at resume falls
        // back to the lexical spelling on both sides, so the relative path (and the card) is
        // still drawn. The key is that relative path, which stays stable across either spelling.
        let relative = if let Ok(relative) = absolute.strip_prefix(&root) {
            relative.to_path_buf()
        } else {
            let lexical_path = normalize(absolute.as_path());
            lexical_path.strip_prefix(&lexical_root).ok()?.to_path_buf()
        };
        if !is_record_path(&relative, tool == "read") {
            return None;
        }
        let stem = absolute
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if stem.is_empty() {
            return None;
        }
        let record_type = match tool {
            "write" => {
                let parsed = arguments
                    .get("content")
                    .and_then(Value::as_str)
                    .and_then(|content| frontmatter_type(content, &stem));
                self.types.insert(relative.clone(), parsed.clone());
                parsed
            }
            "read" => match read_frontmatter(result) {
                // A read that started at the file's first line carries the frontmatter the
                // store saw when the call settled; keep it for a later card.
                Some(text) => {
                    let parsed = frontmatter_type(&text, &stem);
                    self.types.insert(relative.clone(), parsed.clone());
                    parsed
                }
                // A full read from line 1 with no frontmatter shows the store would reject the
                // file: record `None`, so a later patch card shows `?` rather than an older
                // type the transcript can no longer see.
                None if read_started_at_first_line(result) => {
                    self.types.insert(relative.clone(), None);
                    None
                }
                // A partial read (an offset, a directory) establishes nothing either way: the
                // last recorded type stays.
                None => self.types.get(&relative).cloned().unwrap_or(None),
            },
            // A patch edits the file the transcript last saw; the card carries that type.
            _ => self.types.get(&relative).cloned().unwrap_or(None),
        };
        Some(MemoryRef {
            title: stem,
            record_type,
            path: absolute,
            relative,
            read: tool == "read",
        })
    }
}

/// One `.md` file directly under `global/` or `projects/<id>/`. A `read` skips the two index
/// files the host maintains (ADR 0035).
fn is_record_path(relative: &Path, read: bool) -> bool {
    let parts: Vec<&std::ffi::OsStr> = relative.iter().collect();
    let file = match parts.as_slice() {
        [scope, file] if *scope == "global" => *file,
        [scope, _project, file] if *scope == "projects" => *file,
        _ => return false,
    };
    let Some(name) = file.to_str() else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".md") else {
        return false;
    };
    if stem.is_empty() {
        return false;
    }
    if read && matches!(name, "MEMORY.md" | "SESSIONS.md") {
        return false;
    }
    true
}

/// The call's path as an absolute path. A leading `~` expands against `HOME`; a relative path
/// joins the process's current directory (a memory path is never workspace-relative).
fn absolute_path(raw: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = raw.strip_prefix('~') {
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        radian_host::user_home()?.join(rest)
    } else {
        PathBuf::from(raw)
    };
    if path.is_absolute() {
        Some(path)
    } else {
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }
}

/// The memory root with symlinks resolved when it exists, and `.`/`..` collapsed when it does
/// not.
fn canonical_root(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// The call's file with its parent directory resolved through symlinks and the file name kept.
/// Resolving the file itself would change the relative path once the record is deleted, and a
/// resumed transcript must render the same card.
fn record_path(path: &Path) -> PathBuf {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = std::fs::canonicalize(parent)
    {
        return parent.join(name);
    }
    normalize(path)
}

/// The path with `.`/`..` collapsed.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether the read result begins at the file's first line (`1: ...`). A result that starts
/// anywhere else is a partial read.
fn read_started_at_first_line(result: &str) -> bool {
    result
        .split_once(": ")
        .and_then(|(number, _)| number.parse::<u64>().ok())
        == Some(1)
}

/// The frontmatter block of a `read` result, when the read started at the file's first line.
///
/// The `read` tool prefixes every file line with `<n>: ` and may append a continuation hint
/// after the body; the frontmatter is the numbered lines up to and including the closing `---`.
fn read_frontmatter(result: &str) -> Option<String> {
    let mut out = String::new();
    let mut start: Option<u64> = None;
    let mut index: u64 = 0;
    for line in result.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let (number, text) = body.split_once(": ")?;
        let number: u64 = number.parse().ok()?;
        let start = *start.get_or_insert(number);
        if number != start + index {
            return None;
        }
        if index == 0 && (start != 1 || text.trim() != "---") {
            return None;
        }
        out.push_str(text);
        out.push('\n');
        index += 1;
        if index > 1 && text.trim() == "---" {
            return Some(out);
        }
    }
    None
}

/// A record's frontmatter `type`, or `None` when `radian-memory` would call the file
/// unreadable.
///
/// The store's rules apply, rule for rule: a `---` first line, a flat `key: value` frontmatter,
/// a closing `---`, the required `name`, `description`, `type`, `source`, `created` and
/// `modified` fields, both closed sets, the name rule (a `[a-z0-9-]` name of 1 to 64 characters
/// equal to the file stem, ADR 0021 D3), the scalar syntax (plain, single- or double-quoted on
/// one line) and the RFC 3339 UTC timestamps. An unknown top-level key is ignored and an
/// indented line is accepted only inside `provenance`.
fn frontmatter_type(text: &str, stem: &str) -> Option<String> {
    if !name_fits(stem) {
        return None;
    }
    let first = line_at(text, 0);
    if first.text.trim() != "---" {
        return None;
    }
    let mut cursor = first.next;
    let mut fields = Fields::default();
    let mut in_provenance = false;
    loop {
        if cursor >= text.len() {
            return None;
        }
        let line = line_at(text, cursor);
        let trimmed = line.text.trim();
        if trimmed == "---" {
            break;
        }
        if trimmed.is_empty() {
            cursor = line.next;
            continue;
        }
        if line.text.starts_with(' ') || line.text.starts_with('\t') {
            if !in_provenance {
                return None;
            }
            let (_key, value) = line.text.trim().split_once(':')?;
            parse_scalar(value)?;
            cursor = line.next;
            continue;
        }
        in_provenance = false;
        let (key, value) = line.text.split_once(':')?;
        match key.trim() {
            "name" => {
                let value = parse_scalar(value)?;
                if !name_fits(&value) {
                    return None;
                }
                fields.name = Some(value);
            }
            "description" => {
                let value = parse_scalar(value)?;
                if value.contains(['\n', '\r']) {
                    return None;
                }
                fields.description = Some(value);
            }
            "type" => {
                let value = parse_scalar(value)?;
                if !RECORD_TYPES.contains(&value.as_str()) {
                    return None;
                }
                fields.record_type = Some(value);
            }
            "source" => {
                let value = parse_scalar(value)?;
                if !RECORD_SOURCES.contains(&value.as_str()) {
                    return None;
                }
                fields.source = Some(value);
            }
            "created" => {
                let value = parse_scalar(value)?;
                timestamp(&value)?;
                fields.created = Some(value);
            }
            "modified" => {
                let value = parse_scalar(value)?;
                timestamp(&value)?;
                fields.modified = Some(value);
            }
            "last_used" => {
                let value = parse_scalar(value)?;
                if !(value.is_empty() || value == "null" || value == "~") {
                    timestamp(&value)?;
                }
            }
            "provenance" => match value.trim() {
                "" => in_provenance = true,
                "{}" | "null" | "~" => {}
                _ => return None,
            },
            // An unknown top-level key is forward compatible and ignored.
            _ => {}
        }
        cursor = line.next;
    }
    let name = fields.name?;
    fields.description?;
    fields.source?;
    fields.created?;
    fields.modified?;
    let record_type = fields.record_type?;
    (name == stem).then_some(record_type)
}

/// The frontmatter fields the card's parser needs, `None` until each is set.
#[derive(Default)]
struct Fields {
    name: Option<String>,
    description: Option<String>,
    record_type: Option<String>,
    source: Option<String>,
    created: Option<String>,
    modified: Option<String>,
}

/// A record name: `[a-z0-9-]`, 1 to [`NAME_MAX`] characters (ADR 0021 D3).
fn name_fits(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= NAME_MAX
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

/// An RFC 3339 timestamp with a UTC offset (`radian-memory`'s `validate_timestamp`).
fn timestamp(value: &str) -> Option<()> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    (parsed.offset().local_minus_utc() == 0).then_some(())
}

/// One parsed line: its text without the newline and the remaining bytes.
fn take_line(text: &str) -> (&str, &str) {
    match text.find('\n') {
        Some(index) => (&text[..index], &text[index + 1..]),
        None => (text, ""),
    }
}

/// One frontmatter line: its text without the trailing `\r` of a `\r\n` ending and the offset
/// of the next line.
struct Line<'a> {
    next: usize,
    text: &'a str,
}

fn line_at(text: &str, cursor: usize) -> Line<'_> {
    let (raw, rest) = take_line(&text[cursor..]);
    Line {
        next: text.len() - rest.len(),
        text: raw.strip_suffix('\r').unwrap_or(raw),
    }
}

/// Parses a YAML scalar as the store accepts it: a plain scalar, or a single- or double-quoted
/// one on a single line. A plain scalar ends at a ` #` comment and loses trailing whitespace.
fn parse_scalar(value: &str) -> Option<String> {
    let value = value.trim_start();
    if let Some(rest) = value.strip_prefix('"') {
        parse_double_quoted(rest)
    } else if let Some(rest) = value.strip_prefix('\'') {
        parse_single_quoted(rest)
    } else {
        Some(plain_scalar(value))
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

fn parse_double_quoted(rest: &str) -> Option<String> {
    let mut chars = rest.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                return scalar_tail_ok(chars.as_str()).then_some(out);
            }
            '\\' => {
                let escaped = chars.next()?;
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
                    'x' => out.push(parse_hex_escape(&mut chars, 2)?),
                    'u' => out.push(parse_hex_escape(&mut chars, 4)?),
                    'U' => out.push(parse_hex_escape(&mut chars, 8)?),
                    _ => return None,
                }
            }
            _ => out.push(ch),
        }
    }
    None
}

fn parse_hex_escape(chars: &mut std::str::Chars<'_>, digits: usize) -> Option<char> {
    let mut value = 0u32;
    for _ in 0..digits {
        let digit = chars.next()?.to_digit(16)?;
        value = value * 16 + digit;
    }
    char::from_u32(value)
}

fn parse_single_quoted(rest: &str) -> Option<String> {
    let mut chars = rest.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.as_str().starts_with('\'') {
                chars.next();
                out.push('\'');
            } else {
                return scalar_tail_ok(chars.as_str()).then_some(out);
            }
        } else {
            out.push(ch);
        }
    }
    None
}

/// A quoted scalar may be followed only by whitespace and a `#` comment on its line.
fn scalar_tail_ok(tail: &str) -> bool {
    let tail = tail.trim_start();
    tail.is_empty() || tail.starts_with('#')
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEEDBACK: &str = "---\nname: x\ndescription: a fact\ntype: feedback\nsource: model\n\
        created: 2026-10-31T00:00:00Z\nmodified: 2026-10-31T00:00:00Z\n---\nbody\n";

    #[test]
    fn a_frontmatter_names_the_record_and_its_type() {
        assert_eq!(frontmatter_type(FEEDBACK, "x"), Some("feedback".to_owned()));
    }

    #[test]
    fn a_broken_frontmatter_falls_back_to_the_stem() {
        for text in [
            "no frontmatter here\n".to_owned(),
            "---\nname: x\ntype: feedback\n".to_owned(), // never closed
            FEEDBACK.replace("name: x", "name: y"),      // name is not the stem
            FEEDBACK.replace("type: feedback", "type: nonsense"),
            FEEDBACK.replace("description: a fact\n", ""),
            FEEDBACK.replace("source: model\n", ""),
            FEEDBACK.replace("created: 2026-10-31T00:00:00Z\n", ""),
            FEEDBACK.replace("modified: 2026-10-31T00:00:00Z\n", ""),
            FEEDBACK.replace("created: 2026-10-31T00:00:00Z", "created: yesterday"),
            FEEDBACK.replace("source: model", "source: nobody"),
            FEEDBACK.replace("name: x", "name: X"),
        ] {
            assert_eq!(frontmatter_type(&text, "x"), None, "{text:?}");
        }
    }

    /// The card's parser and the store's parser must agree on what is readable: the card shows
    /// a type for exactly the files `radian-memory` would list as a record. The corpus walks
    /// every rule, and the closed sets and the name limit are read from the store itself.
    #[test]
    fn the_frontmatter_rules_match_the_memory_store() {
        assert_eq!(RECORD_TYPES, radian_memory::RECORD_TYPES);
        assert_eq!(RECORD_SOURCES, radian_memory::RECORD_SOURCES);
        assert_eq!(NAME_MAX, radian_memory::NAME_MAX);
        assert_eq!(
            name_fits("x"),
            radian_memory::validate_record_name("x").is_ok()
        );

        let root = std::env::temp_dir().join(format!(
            "radian-tui-memory-card-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(root.join("global")).unwrap();
        let store = radian_memory::FileStore::open(&root);
        let cases: Vec<(&str, String)> = vec![
            ("x", FEEDBACK.to_owned()),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: \"a fact\""),
            ),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: 'a fact'"),
            ),
            ("x", FEEDBACK.replace("type: feedback", "type: user")),
            ("x", FEEDBACK.replace("type: feedback", "type: nonsense")),
            ("x", FEEDBACK.replace("source: model", "source: import")),
            ("x", FEEDBACK.replace("source: model", "source: nobody")),
            (
                "x",
                FEEDBACK.replace(
                    "created: 2026-10-31T00:00:00Z",
                    "created: 2026-10-31T00:00:00+01:00",
                ),
            ),
            (
                "x",
                FEEDBACK.replace("created: 2026-10-31T00:00:00Z", "created: yesterday"),
            ),
            (
                "x",
                FEEDBACK.replace("modified: 2026-10-31T00:00:00Z\n", ""),
            ),
            ("x", FEEDBACK.replace("name: x", "name: y")),
            ("x", FEEDBACK.replace("description: a fact\n", "")),
            // The reviewer's exact case: `name` and `type` only, so the store refuses it.
            ("x", "---\nname: x\ntype: feedback\n---\n".to_owned()),
            (
                "x",
                FEEDBACK.replace("---\nbody", "last_used: null\n---\nbody"),
            ),
            (
                "x",
                FEEDBACK.replace(
                    "---\nbody",
                    "provenance:\n  session_id: s\n  turn_id: t\n---\nbody",
                ),
            ),
            (
                "x",
                FEEDBACK.replace("---\nbody", "note: ignored\n---\nbody"),
            ),
            (
                "x",
                FEEDBACK.replace("---\nbody", "  indented: no\n---\nbody"),
            ),
            ("x", "no frontmatter\n".to_owned()),
            ("x", "---\nname: x\n".to_owned()),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: \"unclosed"),
            ),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: 'unclosed"),
            ),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: quoted\"junk"),
            ),
            (
                "x",
                FEEDBACK.replace("description: a fact", "description: a fact # comment"),
            ),
            ("a-b-1", FEEDBACK.replace("name: x", "name: a-b-1")),
            ("Bad", FEEDBACK.replace("name: x", "name: Bad")),
        ];
        for (stem, text) in cases {
            let path = root.join("global").join(format!("{stem}.md"));
            std::fs::write(&path, &text).unwrap();
            let stored = store
                .read(&radian_memory::Scope::Global, stem)
                .ok()
                .map(|record| record.frontmatter.record_type);
            assert_eq!(
                frontmatter_type(&text, stem),
                stored,
                "stem {stem:?}, text {text:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_card_path_rules_admit_records_only() {
        assert!(is_record_path(Path::new("global/x.md"), false));
        assert!(is_record_path(Path::new("projects/P/x.md"), false));
        assert!(is_record_path(Path::new("global/MEMORY.md"), false));
        assert!(!is_record_path(Path::new("global/MEMORY.md"), true));
        assert!(!is_record_path(Path::new("projects/P/SESSIONS.md"), true));
        assert!(!is_record_path(Path::new("global/deep/x.md"), false));
        assert!(!is_record_path(Path::new("global/x.txt"), false));
        assert!(!is_record_path(Path::new("x.md"), false));
    }

    #[test]
    fn a_read_results_numbering_is_stripped() {
        let numbered = "1: ---\n2: name: x\n3: type: feedback\n4: ---\n5: body\n";
        assert_eq!(
            read_frontmatter(numbered).as_deref(),
            Some("---\nname: x\ntype: feedback\n---\n")
        );
        // A continuation hint after the frontmatter does not matter: the block closes first.
        let continued = format!("{numbered}[truncated: continue with offset=6]\n");
        assert!(read_frontmatter(&continued).is_some());
        // An offset read starts inside the body and carries no frontmatter.
        let offset = "9: body\n10: more\n";
        assert_eq!(read_frontmatter(offset), None);
        // A directory listing is not a numbered file.
        assert_eq!(read_frontmatter("kind size name\n"), None);
    }

    /// A full read that shows no frontmatter clears the type a `write` established, so a later
    /// `patch` card shows `?`; a partial read still keeps it.
    #[test]
    fn a_full_read_with_no_frontmatter_clears_the_recorded_type() {
        let root = std::env::temp_dir().join(format!(
            "radian-tui-memory-read-clear-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(root.join("global")).unwrap();
        let path = root.join("global/x.md");
        let mut records = MemoryRecords::default();
        let written = records.settle(
            Some(&root),
            "write",
            &serde_json::json!({"path": path, "content": FEEDBACK}),
            "wrote 8 lines",
        );
        assert_eq!(written.unwrap().record_type.as_deref(), Some("feedback"));

        let read = records.settle(
            Some(&root),
            "read",
            &serde_json::json!({"path": path}),
            "1: no frontmatter here\n2: body\n",
        );
        assert_eq!(
            read.unwrap().record_type,
            None,
            "a full read proves no frontmatter"
        );
        let patched = records.settle(
            Some(&root),
            "patch",
            &serde_json::json!({"path": path, "old": "body", "new": "changed"}),
            "replaced 1 occurrence",
        );
        assert_eq!(patched.unwrap().type_label(), "?");

        // A partial read leaves the last recorded type alone.
        let mut records = MemoryRecords::default();
        records.settle(
            Some(&root),
            "write",
            &serde_json::json!({"path": path, "content": FEEDBACK}),
            "wrote 8 lines",
        );
        records.settle(
            Some(&root),
            "read",
            &serde_json::json!({"path": path, "offset": 9}),
            "9: body\n",
        );
        let patched = records.settle(
            Some(&root),
            "patch",
            &serde_json::json!({"path": path, "old": "body", "new": "changed"}),
            "replaced 1 occurrence",
        );
        assert_eq!(patched.unwrap().type_label(), "feedback");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A record whose parent directory is gone still draws its card, with its type, at resume:
    /// the canonical pair falls back to the lexical spelling on both sides, and the transcript
    /// keys the recorded type by the relative path (ADR 0035: the card never comes from the
    /// file).
    #[test]
    fn a_record_whose_parent_directory_is_gone_still_draws_its_card() {
        let dir = std::env::temp_dir().join(format!(
            "radian-tui-memory-gone-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("real/projects/P")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let root = dir.join("link");
        let path = root.join("projects/P/x.md");
        let mut records = MemoryRecords::default();
        let written = records.settle(
            Some(&root),
            "write",
            &serde_json::json!({"path": path, "content": FEEDBACK}),
            "wrote 8 lines",
        );
        let written = written.expect("the write is a record");
        assert_eq!(written.relative, Path::new("projects/P/x.md"));
        assert_eq!(written.type_label(), "feedback");

        // The whole project directory disappears, not just the file.
        std::fs::remove_dir_all(dir.join("real/projects/P")).unwrap();
        let patched = records.settle(
            Some(&root),
            "patch",
            &serde_json::json!({"path": path, "old": "body", "new": "changed"}),
            "replaced 1 occurrence",
        );
        let patched = patched.expect("the recorded path still names a record");
        assert_eq!(patched.relative, Path::new("projects/P/x.md"));
        assert_eq!(patched.type_label(), "feedback");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The card's path is the same before and after the record disappears: the parent (which
    /// exists) is resolved and the file name is kept (ADR 0035: a resume redraws the card).
    #[test]
    fn a_deleted_record_keeps_its_relative_path() {
        let root = std::env::temp_dir().join(format!(
            "radian-tui-memory-path-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(root.join("global")).unwrap();
        let file = root.join("global/x.md");
        std::fs::write(&file, "body\n").unwrap();
        let live = record_path(&file);
        let relative = live
            .strip_prefix(canonical_root(&root))
            .expect("the record is under the root");
        assert_eq!(relative, Path::new("global/x.md"));
        std::fs::remove_file(&file).unwrap();
        assert_eq!(record_path(&file), live, "the path changed after deletion");
        let _ = std::fs::remove_dir_all(&root);
    }
}
