//! Session listing, search and export, shared by `radian-exec` and `radian-tui` (Phase 4).
//!
//! The data part lives here so the two hosts cannot drift: which sessions belong to a
//! workspace, what a one-line summary is, what a search hit looks like, and how a session is
//! rendered to Markdown/JSON. The hosts own only their flags, their output formatting and the
//! terminal. Nothing here writes a session file: the JSONL files are the authority (ADR 0006
//! §10) and the index is a disposable cache.

use std::collections::HashMap;
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

/// Whether two workspace values name the same directory. Each side is canonicalized when the
/// filesystem can, so macOS's `/var` → `/private/var` symlink, a symlinked project directory,
/// and a session recorded under either spelling are one workspace. A side that cannot be
/// canonicalized (a recorded directory that no longer exists) is compared as written.
fn same_directory(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The sessions of `workspace`, newest activity first, with their summaries. The index stores
/// the workspace as recorded, so the filter runs in Rust and compares directories: `--list`
/// must show a session `--resume` accepts, even when it was recorded under another spelling of
/// the same directory.
pub fn list(index: &Index, workspace: &str) -> Result<Vec<SessionRow>, String> {
    let t = std::time::Instant::now();
    let sessions: Vec<_> = index
        .list(None)
        .map_err(|e| format!("cannot list sessions: {e}"))?
        .into_iter()
        .filter(|session| {
            session
                .workspace
                .as_deref()
                .is_some_and(|recorded| same_directory(recorded, workspace))
        })
        .collect();
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
    // The index filters by the stored string, which misses another spelling of the same
    // directory. Take the matching sessions (and their timestamps) first, then search once per
    // recorded spelling they use. A session's hits all come from one call, and a stable sort by
    // session id restores the index's `(session_id, seq)` order across them.
    let sessions: Vec<_> = index
        .list(None)
        .map_err(|e| format!("cannot list sessions: {e}"))?
        .into_iter()
        .filter(|session| {
            session
                .workspace
                .as_deref()
                .is_some_and(|recorded| same_directory(recorded, workspace))
        })
        .collect::<Vec<_>>();
    let mut spellings: Vec<String> = sessions
        .iter()
        .filter_map(|session| session.workspace.clone())
        .collect();
    spellings.sort();
    spellings.dedup();
    let mut hits = Vec::new();
    for spelling in &spellings {
        hits.extend(
            index
                .search(query, Some(spelling), limit)
                .map_err(|e| format!("cannot search: {e}"))?,
        );
    }
    hits.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    hits.truncate(limit);
    // `SearchHit` does not carry the session's timestamp; take it from the same list `--list`
    // uses. Building it once per search keeps a hit's row aligned with the list view.
    let times: HashMap<String, Option<String>> = sessions
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
    if !in_workspace(&session, workspace) {
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

/// Whether a session belongs to `workspace`: its `SessionCreated.workspace` names the same
/// directory. The recorded value may be an older spelling of the process workspace (a
/// pre-canonicalisation build, a symlinked home or project, macOS's `/var` → `/private/var`),
/// so the two sides are compared after canonicalizing each (see [`same_directory`]).
///
/// This is the rule [`export`] refuses on and `radian acp`'s `session/load` applies before it
/// opens the journal.
pub fn in_workspace(session: &Session, workspace: &str) -> bool {
    session_workspace(session).is_some_and(|recorded| same_directory(&recorded, workspace))
}

/// The workspace recorded by the session file `session_id` in `dir`, read without opening the
/// journal for appending. `None` when the id is not a plain file name, the file cannot be
/// read, or it carries no `SessionCreated`: the session opener reports those with its own
/// message (an unknown id must name the id).
pub fn file_workspace(dir: &Path, session_id: &str) -> Option<String> {
    if crate::validate_session_id(session_id).is_err() {
        return None;
    }
    let path = dir.join(format!("{session_id}.jsonl"));
    let session = Session::open(&path).ok()?;
    session_workspace(&session)
}

/// The refusal for the session file `session_id` in `dir` when it belongs to **another**
/// workspace, or `None` when it belongs to `workspace` or [`file_workspace`] has no answer (the
/// opener reports those). The message is [`workspace_mismatch`]'s, so `--resume` and
/// `session/load` say the same thing.
pub fn other_workspace(dir: &Path, session_id: &str, workspace: &str) -> Option<String> {
    let recorded = file_workspace(dir, session_id)?;
    workspace_mismatch(session_id, &recorded, workspace)
}

/// The refusal for a session whose `SessionCreated.workspace` is `recorded` while the process
/// workspace is `workspace`; `None` when they name the same directory (see [`same_directory`]).
/// The message prints the recorded value as it was recorded. `--resume` and `radian acp`'s
/// `session/load` both answer with this message; `--export` applies the same rule with its own
/// wording.
pub fn workspace_mismatch(session_id: &str, recorded: &str, workspace: &str) -> Option<String> {
    (!same_directory(recorded, workspace)).then(|| {
        format!(
            "session {session_id:?} is not in this workspace; it was created in {recorded:?} and \
             is only opened there"
        )
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

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "radian-host-sessions-unit-{}-{label}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A readable session file whose `SessionCreated.workspace` is `workspace` verbatim.
    fn write_session(dir: &Path, id: &str, workspace: &str, user_text: &str) -> std::path::PathBuf {
        let ts = "2026-10-13T00:00:00.000000Z";
        let lines = [
            serde_json::json!({"schema_version":1,"session_id":id,"seq":0,"ts":ts,
                "event":"SessionCreated","critical":true,
                "data":{"workspace":workspace,"config_snapshot":{}}}),
            serde_json::json!({"schema_version":1,"session_id":id,"seq":1,"ts":ts,
                "event":"MessageAppended","critical":true,
                "data":{"index":0,"message":{"user":{"content":[{"text":{"text":user_text}}]}}}}),
        ];
        let mut text = String::new();
        for line in lines {
            text.push_str(&line.to_string());
            text.push('\n');
        }
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// `<tmp>/real/ws` and `<tmp>/link/ws` where `link -> real`, as lossy strings.
    fn symlink_pair(dir: &TempDir) -> (String, String) {
        let real = dir.path().join("real").join("ws");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path().join("real"), &link).unwrap();
        let linked_ws = link.join("ws");
        (
            linked_ws.to_string_lossy().into_owned(),
            real.to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn a_symlinked_workspace_spelling_is_not_a_mismatch() {
        let dir = TempDir::new("symlink-belongs");
        let (recorded, process) = symlink_pair(&dir);
        assert_eq!(
            workspace_mismatch("s", &recorded, &process),
            None,
            "{recorded} and {process} are the same directory"
        );
    }

    #[test]
    fn a_recorded_workspace_that_is_gone_is_refused_as_written() {
        let dir = TempDir::new("gone");
        let process = dir.path().join("real");
        std::fs::create_dir_all(&process).unwrap();
        let missing = dir.path().join("gone");
        let missing_text = missing.to_string_lossy().into_owned();
        let process_text = process.to_string_lossy().into_owned();
        let message = workspace_mismatch("s", &missing_text, &process_text)
            .expect("a recorded directory that is gone cannot be the process workspace");
        assert!(
            message.contains(&format!("{missing_text:?}")),
            "the message must name the recorded path as written: {message}"
        );
    }

    #[test]
    fn two_different_existing_directories_are_refused() {
        let dir = TempDir::new("different");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert!(workspace_mismatch("s", &a.to_string_lossy(), &b.to_string_lossy()).is_some());
    }

    #[test]
    fn in_workspace_follows_the_symlinked_spelling() {
        let dir = TempDir::new("in-workspace");
        let (recorded, process) = symlink_pair(&dir);
        let path = write_session(dir.path(), "link-ws", &recorded, "hello");
        let session = Session::open(&path).unwrap();
        assert!(in_workspace(&session, &process));
    }

    #[test]
    fn list_finds_a_session_recorded_under_a_symlinked_spelling() {
        let dir = TempDir::new("list-symlink");
        let (recorded, process) = symlink_pair(&dir);
        write_session(dir.path(), "link-ws", &recorded, "hello");
        let index = open_index(dir.path()).unwrap().unwrap();
        let rows = list(&index, &process).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].session_id, "link-ws");
    }

    #[test]
    fn search_finds_a_session_recorded_under_a_symlinked_spelling() {
        let dir = TempDir::new("search-symlink");
        let (recorded, process) = symlink_pair(&dir);
        write_session(dir.path(), "link-ws", &recorded, "find me here");
        let index = open_index(dir.path()).unwrap().unwrap();
        let hits = search(&index, &process, "find me", 10).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].session_id, "link-ws");
    }

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
