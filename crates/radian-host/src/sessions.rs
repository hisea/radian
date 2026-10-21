//! Session listing, search and export, shared by `radian-exec` and `radian-tui` (Phase 4).
//!
//! The data part lives here so the two hosts cannot drift: which sessions belong to a
//! workspace, what a one-line summary is, what a search hit looks like, and how a session is
//! rendered to Markdown/JSON. The hosts own only their flags, their output formatting and the
//! terminal. Nothing here writes a session file: the JSONL files are the authority (ADR 0006
//! §10) and the index is a disposable cache.

use std::path::Path;

use radian_session::{ExportOptions, Index, Session, export_json, export_markdown};

/// The maximum number of search hits a host asks for in one call.
pub const SEARCH_LIMIT: usize = 200;

/// The maximum length of a one-line session summary, in characters.
pub const SUMMARY_MAX_CHARS: usize = 80;

/// What to render for an export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
}

impl Format {
    /// The file extension an export of this format gets.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Markdown => "md",
            Self::Json => "json",
        }
    }
}

/// How an export is rendered: which optional payloads are included and which values are
/// redacted. The flags mirror `radian exec --export`; redaction is not optional (ADR 0013
/// decision 3).
#[derive(Clone, Debug, Default)]
pub struct ExportSettings {
    pub include_reasoning: bool,
    pub include_provider_payload: bool,
    /// Values that must never appear in the output (the key and every header value).
    pub secrets: Vec<String>,
}

/// One session of a workspace, as a picker or `--list` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub created_ts: Option<String>,
    pub last_ts: Option<String>,
    /// The number of committed `MessageAppended` records.
    pub message_count: u64,
    /// The first user message as one line, at most [`SUMMARY_MAX_CHARS`] characters.
    pub summary: String,
}

/// One search hit, ready to print or show. `snippet` is one line; it is **not** redacted (the
/// caller redacts with its own secret list).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRow {
    pub session_id: String,
    pub last_ts: Option<String>,
    pub snippet: String,
}

/// Opens the workspace's session index and rebuilds it from the JSONL files. `Ok(None)` means
/// the session directory does not exist yet (nothing was ever created).
pub fn open_index(dir: &Path) -> Result<Option<Index>, String> {
    if !dir.exists() {
        return Ok(None);
    }
    radian_session::warn_if_wide(dir);
    let t_open = std::time::Instant::now();
    let mut index = Index::open(dir.join("index.db"))
        .map_err(|e| format!("cannot open the session index in {}: {e}", dir.display()))?;
    radian_session::trace::took("picker/index-open", t_open);
    let t_rebuild = std::time::Instant::now();
    index
        .rebuild_dir(dir)
        .map_err(|e| format!("cannot rebuild the session index: {e}"))?;
    radian_session::trace::took("picker/index-rebuild", t_rebuild);
    Ok(Some(index))
}

/// The sessions of `workspace`, newest activity first, with their summaries.
pub fn list(index: &Index, workspace: &str) -> Result<Vec<SessionRow>, String> {
    let t = std::time::Instant::now();
    let sessions = index
        .list(Some(workspace))
        .map_err(|e| format!("cannot list sessions: {e}"))?;
    radian_session::trace::took("picker/list", t);
    let t_sum = std::time::Instant::now();
    let out: Result<Vec<SessionRow>, String> = sessions
        .into_iter()
        .map(|session| {
            let (message_count, summary) = summarize(index, &session.session_id);
            Ok(SessionRow {
                session_id: session.session_id,
                created_ts: session.created_ts,
                last_ts: session.last_ts,
                message_count,
                summary,
            })
        })
        .collect();
    radian_session::trace::took("picker/summarize", t_sum);
    out
}

/// Substring search over `workspace`'s message text. `snippet` comes back as one line; the
/// caller redacts it. `last_ts` is the same value `list` shows, so hits can be told apart.
pub fn search(
    index: &Index,
    workspace: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchRow>, String> {
    let hits = index
        .search(query, Some(workspace), limit)
        .map_err(|e| format!("cannot search: {e}"))?;
    // `SearchHit` does not carry the session's timestamp; take it from the same list `--list`
    // uses. Building it once per search keeps a hit's row aligned with the list view.
    let times: std::collections::HashMap<String, Option<String>> = index
        .list(Some(workspace))
        .map_err(|e| format!("cannot list sessions: {e}"))?
        .into_iter()
        .map(|session| (session.session_id, session.last_ts))
        .collect();
    Ok(hits
        .into_iter()
        .map(|hit| SearchRow {
            last_ts: times.get(&hit.session_id).cloned().flatten(),
            session_id: hit.session_id,
            snippet: one_line(&hit.snippet),
        })
        .collect())
}

/// Renders one session of `workspace` to a string, redacting `settings.secrets`. The id is
/// validated first, so `../x` cannot name a file outside the session directory; the session
/// must belong to `workspace`, the same rule `--list` uses. Reads the JSONL directly (no
/// index), so this writes nothing.
pub fn export(
    dir: &Path,
    workspace: &str,
    session_id: &str,
    format: Format,
    settings: &ExportSettings,
) -> Result<String, String> {
    crate::validate_session_id(session_id)?;
    let path = dir.join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Err(format!("no session {session_id:?} in {}", dir.display()));
    }
    let session =
        Session::open(&path).map_err(|e| format!("cannot read session {session_id:?}: {e}"))?;
    if session_workspace(&session).as_deref() != Some(workspace) {
        return Err(format!(
            "session {session_id:?} is not in this workspace; only the current workspace can be \
             exported"
        ));
    }
    let mut opts = ExportOptions::new()
        .include_reasoning(settings.include_reasoning)
        .include_provider_payload(settings.include_provider_payload);
    for secret in &settings.secrets {
        opts = opts.with_secret(secret.clone());
    }
    match format {
        Format::Markdown => export_markdown(&session, &opts),
        Format::Json => export_json(&session, &opts),
    }
    .map_err(|e| format!("cannot export session {session_id:?}: {e}"))
}

/// The workspace recorded in a session's `SessionCreated` event. `None` for a file the reader
/// accepted but that has no `SessionCreated`.
pub fn session_workspace(session: &Session) -> Option<String> {
    use radian_session::EventData;
    session.events().iter().find_map(|event| match &event.data {
        EventData::Other(data) => data
            .get("workspace")
            .and_then(|workspace| workspace.as_str())
            .map(str::to_owned),
        _ => None,
    })
}

/// The `message_count` and one-line `summary` of one session, both derived from its index
/// rows. A session that fails to list gets zero and an empty summary rather than failing the
/// whole call: the row is still useful.
fn summarize(index: &Index, session_id: &str) -> (u64, String) {
    let events = index.indexed_events(session_id).unwrap_or_default();
    let count = events
        .iter()
        .filter(|event| event.event == "MessageAppended")
        .count() as u64;
    let first = events
        .iter()
        .find(|event| event.event == "MessageAppended" && event.message_index == Some(0))
        .and_then(|event| event.text.as_deref())
        .unwrap_or("");
    (count, truncate_summary(&one_line(first)))
}

/// Every run of whitespace (including the newlines a multi-line message carries) becomes one
/// space.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cuts a one-line summary to at most [`SUMMARY_MAX_CHARS`] characters, appending `…` when it
/// had to cut. Truncating by `char` never splits a multi-byte character.
fn truncate_summary(text: &str) -> String {
    if text.chars().count() <= SUMMARY_MAX_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(SUMMARY_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_is_one_line_and_capped() {
        assert_eq!(one_line("a\n b\tc"), "a b c");
        assert_eq!(truncate_summary("short"), "short");
        let long = "x".repeat(SUMMARY_MAX_CHARS + 10);
        let cut = truncate_summary(&long);
        assert_eq!(cut.chars().count(), SUMMARY_MAX_CHARS);
        assert!(cut.ends_with('…'), "{cut}");
        // Multi-byte text is cut by character, not byte.
        let mut text = "é".repeat(SUMMARY_MAX_CHARS);
        text.push('尾');
        let cut = truncate_summary(&text);
        assert_eq!(cut.chars().count(), SUMMARY_MAX_CHARS);
    }

    #[test]
    fn an_export_rejects_an_id_that_is_not_a_file_name() {
        let dir = std::env::temp_dir();
        let settings = ExportSettings::default();
        let error = export(&dir, "/w", "../x", Format::Markdown, &settings).unwrap_err();
        assert!(error.contains("session id"), "{error}");
    }
}
