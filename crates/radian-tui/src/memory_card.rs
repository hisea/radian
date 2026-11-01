//! What a tool card shows when the model touches a memory file (ADR 0021, ADR 0035).
//!
//! The transcript resolves a memory path once, when a tool call's result arrives: the record's
//! frontmatter `name` and `type` (or the file stem and `?` when the frontmatter does not
//! parse), its absolute path for the opener, and its path relative to the memory root for the
//! card. [`crate::tools::ToolCard`] carries the result; the renderers and the block layout are
//! pure and only read it.
//!
//! A `write` carries the whole file in its `content` argument, so its frontmatter is read from
//! the call itself. A `patch` and a `read` name an existing file, whose bytes are read once
//! here. The path rules mirror the store's (`radian-memory`'s `FileStore`): one `.md` file
//! directly under `global/` or `projects/<id>/`, never a deeper directory.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// The closed `type` set of a record (`radian-memory`'s `RECORD_TYPES`). Duplicated here
/// because the TUI does not depend on the memory crate at run time and cannot edit it.
const RECORD_TYPES: &[&str] = &["user", "feedback", "project", "reference"];

/// One memory file a successful call touched, as the transcript draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRef {
    /// The frontmatter `name`, or the file stem when the frontmatter does not parse.
    pub title: String,
    /// The frontmatter `type`; `None` when the frontmatter does not parse (the card shows `?`).
    pub record_type: Option<String>,
    /// The absolute file, for the opener.
    pub path: PathBuf,
    /// The path relative to the memory root, for the card (`projects/<id>/x.md`).
    pub relative: PathBuf,
    /// Whether this is a `read` (one line) rather than a `write`/`patch` (one card).
    pub read: bool,
}

impl MemoryRef {
    /// The type the card shows: the closed-set value, or `?` when the frontmatter does not
    /// parse.
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

/// Resolves one tool call to a memory file, or `None` when its path is not a record.
///
/// `root` is the memory root (the parent of `global/` and `projects/`); `tool` is the exact
/// built-in name (`read`, `write` or `patch`); `arguments` is the call's JSON. A `write`'s
/// frontmatter comes from its `content`; a `patch`'s and a `read`'s from the file on disk.
pub fn detect(root: Option<&Path>, tool: &str, arguments: &Value) -> Option<MemoryRef> {
    let root = root?;
    if !matches!(tool, "read" | "write" | "patch") {
        return None;
    }
    let raw = arguments.get("path").and_then(Value::as_str)?;
    let absolute = absolute_path(raw)?;
    let root = canonical_or_normalized(root);
    let absolute = canonical_or_normalized(&absolute);
    let relative = absolute.strip_prefix(&root).ok()?.to_path_buf();
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
    let (title, record_type) = if tool == "write" {
        arguments
            .get("content")
            .and_then(Value::as_str)
            .and_then(|content| frontmatter(content, &stem))
            .map_or((stem.clone(), None), |(title, kind)| (title, Some(kind)))
    } else {
        std::fs::read_to_string(&absolute)
            .ok()
            .and_then(|text| frontmatter(&text, &stem))
            .map_or((stem.clone(), None), |(title, kind)| (title, Some(kind)))
    };
    Some(MemoryRef {
        title,
        record_type,
        path: absolute,
        relative,
        read: tool == "read",
    })
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

/// The path with symlinks resolved when it exists, and `.`/`..` collapsed when it does not.
fn canonical_or_normalized(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
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
    })
}

/// The record's `name` and `type`, or `None` when the frontmatter does not parse. The store's
/// rules apply: a `---` line, a flat `key: value` body, a closing `---`, a `name` equal to the
/// file stem, and a `type` inside the closed set.
fn frontmatter(text: &str, stem: &str) -> Option<(String, String)> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    let mut name = None;
    let mut kind = None;
    let mut closed = false;
    for line in lines {
        let line = line.trim_end();
        if line == "---" {
            closed = true;
            break;
        }
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(unquote(value.trim()));
        } else if let Some(value) = line.strip_prefix("type:") {
            kind = Some(unquote(value.trim()));
        }
    }
    if !closed {
        return None;
    }
    let name = name?;
    let kind = kind?;
    if name != stem || name.is_empty() || !RECORD_TYPES.contains(&kind.as_str()) {
        return None;
    }
    Some((name, kind))
}

/// A scalar's value with one matching pair of single or double quotes removed.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_owned()
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frontmatter_names_the_record_and_its_type() {
        let text = "---\nname: x\ndescription: d\ntype: feedback\nsource: model\n---\nbody\n";
        assert_eq!(
            frontmatter(text, "x"),
            Some(("x".to_owned(), "feedback".to_owned()))
        );
    }

    #[test]
    fn a_broken_frontmatter_falls_back_to_the_stem() {
        for text in [
            "no frontmatter here\n",
            "---\nname: x\ntype: feedback\n",      // never closed
            "---\nname: y\ntype: feedback\n---\n", // name is not the stem
            "---\nname: x\ntype: nonsense\n---\n", // type outside the set
            "---\nname: x\n---\n",                 // no type
        ] {
            assert_eq!(frontmatter(text, "x"), None, "{text:?}");
        }
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
}
