//! The host-maintained session index, `SESSIONS.md`, in a project scope (ADR 0021 D3).
//!
//! One line per ended session, newest last:
//!
//! ```text
//! - <session id> <start time, RFC 3339 UTC> <workspace path relative to the repository root, or .> — <first line of the first user message, cut at 120 characters, newlines removed>
//! ```
//!
//! The host appends a line only when a session ends, from the committed journal (ADR 0019):
//! a session with no committed user message writes nothing. A line for the same session id is
//! replaced in place, so a resumed session never duplicates its line. After an append the
//! oldest lines beyond `[memory] sessions_index_max_lines` are dropped; the whole file is
//! rewritten through the store's temporary file and atomic rename.
//!
//! The file is low-trust data and read-only to the model (ADR 0021 invariant 4): `write` and
//! `patch` refuse it, in both tiers.

use std::path::Path;

use radian_memory::{DESCRIPTION_MAX, FileStore, MemoryError, ProjectId, Scope};

use super::service::MemoryService;

/// `[memory] sessions_index_max_lines`'s default (ADR 0021 D3).
pub const DEFAULT_SESSIONS_INDEX_MAX_LINES: u64 = 1000;

/// One session as the index records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub session_id: String,
    /// The `SessionCreated` event's RFC 3339 timestamp.
    pub started_at: String,
    /// The workspace relative to the repository root, or `.`.
    pub workspace: String,
    /// The first line of the first user message.
    pub first_message: String,
}

impl SessionEntry {
    /// The one line the index holds.
    pub fn line(&self) -> String {
        session_line(
            &self.session_id,
            &self.started_at,
            &self.workspace,
            &self.first_message,
        )
    }
}

/// The line the index holds: `- <id> <start> <workspace> — <first line>`.
pub fn session_line(
    session_id: &str,
    started_at: &str,
    workspace: &str,
    first_message: &str,
) -> String {
    format!(
        "- {session_id} {started_at} {workspace} — {}",
        first_line_cut(first_message)
    )
}

/// The first line of `message`, with a `\r\n` pair read as one line, cut at
/// [`DESCRIPTION_MAX`] characters (the same 120 the memory index uses).
pub fn first_line_cut(message: &str) -> String {
    message
        .split(['\n', '\r'])
        .next()
        .unwrap_or_default()
        .chars()
        .take(DESCRIPTION_MAX)
        .collect()
}

/// The index text after `line` is appended or replaces the line of `session_id`, capped at
/// `max_lines` lines (oldest first out). A line belongs to a session when its first token after
/// `- ` equals the id.
pub fn upsert_session_line(text: &str, session_id: &str, line: &str, max_lines: u64) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    match lines
        .iter()
        .position(|candidate| owns(candidate, session_id))
    {
        Some(index) => lines[index] = line,
        None => lines.push(line),
    }
    let max_lines = max_lines.max(1) as usize;
    if lines.len() > max_lines {
        let drop = lines.len() - max_lines;
        lines.drain(0..drop);
    }
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn owns(line: &str, session_id: &str) -> bool {
    line.strip_prefix("- ")
        .and_then(|rest| rest.split_once(' '))
        .is_some_and(|(id, _)| id == session_id)
}

/// Appends `entry` to the project's session index, creating it when missing and capping it at
/// `max_lines`. A workspace that maps to no project is not written and creates no project
/// (ADR 0021 D2): the caller passes a known project.
pub fn append_session(
    files: &FileStore,
    project: &ProjectId,
    entry: &SessionEntry,
    max_lines: u64,
) -> Result<(), MemoryError> {
    let scope = Scope::Project(project.clone());
    let text = files.read_sessions_index(&scope)?;
    let updated = upsert_session_line(&text, &entry.session_id, &entry.line(), max_lines);
    if updated != text {
        files.write_sessions_index(&scope, &updated)?;
    }
    Ok(())
}

/// The workspace column: the workspace relative to the repository root, or `.` when it is the
/// root (or when the directory has no repository root at all).
pub fn workspace_column(workspace: &Path) -> String {
    let Some(root) = super::identity::repository_root(workspace) else {
        return ".".to_owned();
    };
    let workspace = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    match workspace.strip_prefix(&root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => ".".to_owned(),
    }
}

/// Appends the ended session's line when this workspace maps to a project. Reads and session
/// ends never create a project; an unlinked workspace, or a session with no committed user
/// message, writes nothing.
///
/// `history` is the committed transcript (ADR 0019), `started_at` the `SessionCreated`
/// timestamp, and `session_id` the journal's own id.
pub fn record_ended_session(
    service: &MemoryService,
    session_id: &str,
    started_at: &str,
    workspace: &Path,
    history: &[radian_core::Message],
    max_lines: u64,
) -> Result<bool, MemoryError> {
    let project = match service.state() {
        Ok(super::service::ProjectState::Known(project)) => project,
        Ok(super::service::ProjectState::Unlinked { .. }) => return Ok(false),
        Err(error) => {
            tracing::warn!("memory project state could not be read: {error}");
            return Ok(false);
        }
    };
    let Some(first) = history.iter().find(|message| {
        matches!(
            message,
            radian_core::Message::User { meta, .. }
                if meta.source == Some(radian_core::InputSource::User)
        )
    }) else {
        return Ok(false);
    };
    let entry = SessionEntry {
        session_id: session_id.to_owned(),
        started_at: started_at.to_owned(),
        workspace: workspace_column(workspace),
        first_message: first.text(),
    };
    append_session(&service.files(), &project, &entry, max_lines)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn entry(id: &str, workspace: &str, message: &str) -> SessionEntry {
        SessionEntry {
            session_id: id.to_owned(),
            started_at: "2026-10-31T00:00:00.000000Z".to_owned(),
            workspace: workspace.to_owned(),
            first_message: message.to_owned(),
        }
    }

    fn project_files(dir: &TempDir) -> (FileStore, ProjectId) {
        let files = FileStore::open(dir.path().join("memory"));
        let project = radian_memory::ProjectId("p-one".into());
        (files, project)
    }

    /// Edge 6: a newline and 200 characters still make one line, cut at 120.
    #[test]
    fn the_line_takes_the_first_line_and_cuts_it_at_120_characters() {
        let message = format!("{}\nsecond line", "x".repeat(200));
        let line = session_line("sid-1", "2026-10-31T00:00:00.000000Z", "crates/x", &message);
        assert_eq!(
            line,
            format!(
                "- sid-1 2026-10-31T00:00:00.000000Z crates/x — {}",
                "x".repeat(120)
            )
        );
        assert!(!line.contains('\n'), "the line holds no newline: {line}");
        assert_eq!(first_line_cut("one\r\ntwo"), "one");
    }

    /// Edge 4 (unit half): the same session id replaces its line instead of adding one.
    #[test]
    fn a_second_line_for_the_same_session_replaces_the_first() {
        let first = entry("sid-1", ".", "first");
        let mut text = upsert_session_line("", &first.session_id, &first.line(), 1000);
        let second = entry("sid-1", "crates/x", "changed");
        text = upsert_session_line(&text, &second.session_id, &second.line(), 1000);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains("crates/x — changed"),
            "the new line replaced the old: {text}"
        );
    }

    /// Edges 5 and the cap: 1001 appended lines with the default cap leave the newest 1000.
    #[test]
    fn the_cap_drops_the_oldest_lines_and_keeps_the_newest_last() {
        let mut text = String::new();
        for index in 0..=1000 {
            let line = entry(&format!("sid-{index:04}"), ".", &format!("message {index}"));
            text = upsert_session_line(
                &text,
                &line.session_id,
                &line.line(),
                DEFAULT_SESSIONS_INDEX_MAX_LINES,
            );
        }
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1000);
        assert!(
            !lines.iter().any(|line| line.starts_with("- sid-0000 ")),
            "the oldest line is gone"
        );
        assert!(
            lines.last().unwrap().starts_with("- sid-1000 "),
            "the newest line is last: {:?}",
            lines.last()
        );
        assert!(lines.first().unwrap().starts_with("- sid-0001 "));
    }

    /// A missing file is created, and the write goes through the store's atomic path.
    #[test]
    fn append_creates_the_file_and_reads_back_the_line() {
        let dir = TempDir::with_label("radian-host-sessions", "append");
        let (files, project) = project_files(&dir);
        let entry = entry("sid-1", ".", "first task");
        append_session(&files, &project, &entry, 1000).unwrap();
        let text = files
            .read_sessions_index(&Scope::Project(project.clone()))
            .unwrap();
        assert_eq!(text, format!("{}\n", entry.line()));
        assert!(files.sessions_index_path(&Scope::Project(project)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_listing_does_not_report_the_session_index_as_unreadable() {
        let dir = TempDir::with_label("radian-host-sessions", "list");
        let (files, project) = project_files(&dir);
        let entry = entry("sid-1", ".", "first task");
        append_session(&files, &project, &entry, 1000).unwrap();
        assert!(
            files
                .list(&Scope::Project(project.clone()))
                .unwrap()
                .is_empty(),
            "SESSIONS.md is not a record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_workspace_column_is_dot_at_the_root_and_relative_below_it() {
        let dir = TempDir::with_label("radian-host-sessions", "workspace");
        let root = dir.path().join("repo");
        let nested = root.join("crates").join("x");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(workspace_column(&root), ".");
        assert_eq!(workspace_column(&nested), "crates/x");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
