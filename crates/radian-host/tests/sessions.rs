//! `radian_host::sessions`: the shared list/search/export data layer (Phase 4). Builds real
//! JSONL session files (the authority) and checks the workspace filter, the summary, search
//! and export.

use std::path::{Path, PathBuf};

use radian_host::sessions::{self, ExportSettings, Format, SUMMARY_MAX_CHARS};
use serde_json::json;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-host-sessions-{}-{label}-{n}",
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

/// A session with a first user message and an assistant reply. The workspace is taken
/// verbatim (the index stores it as written).
fn write_session(dir: &Path, id: &str, workspace: &str, user_text: &str) -> PathBuf {
    let ts = "2026-10-13T00:00:00.000000Z";
    let lines = [
        json!({"schema_version":1,"session_id":id,"seq":0,"ts":ts,"event":"SessionCreated",
               "critical":true,"data":{"workspace":workspace,"config_snapshot":{}}}),
        json!({"schema_version":1,"session_id":id,"seq":1,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":0,
               "message":{"user":{"content":[{"text":{"text":user_text}}]}}}}),
        json!({"schema_version":1,"session_id":id,"seq":2,"ts":ts,"event":"MessageAppended",
               "critical":true,"data":{"index":1,
               "message":{"assistant":{"content":[{"text":{"text":"reply"}}]}}}}),
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

#[test]
fn list_filters_by_workspace_and_summarizes() {
    let dir = TempDir::new("list");
    write_session(dir.path(), "in-ws", "/work/ws", "hello from this workspace");
    write_session(dir.path(), "other", "/work/other", "hello from elsewhere");
    let index = sessions::open_index(dir.path()).unwrap().unwrap();
    let rows = sessions::list(&index, "/work/ws").unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].session_id, "in-ws");
    assert_eq!(rows[0].message_count, 2);
    assert_eq!(rows[0].summary, "hello from this workspace");
}

#[test]
fn the_summary_is_capped_at_eighty_characters() {
    let dir = TempDir::new("truncate");
    let long = "é".repeat(200);
    write_session(dir.path(), "long", "/work/ws", &long);
    let index = sessions::open_index(dir.path()).unwrap().unwrap();
    let rows = sessions::list(&index, "/work/ws").unwrap();
    assert_eq!(rows[0].summary.chars().count(), SUMMARY_MAX_CHARS);
    assert!(rows[0].summary.ends_with('…'));
}

#[test]
fn a_missing_directory_has_no_index_and_an_empty_one_lists_nothing() {
    let dir = TempDir::new("empty");
    // Never created: no index at all.
    assert!(
        sessions::open_index(&dir.path().join("nope"))
            .unwrap()
            .is_none()
    );
    // Existing but empty: an index with no rows.
    let index = sessions::open_index(dir.path()).unwrap().unwrap();
    assert!(sessions::list(&index, "/work/ws").unwrap().is_empty());
}

#[test]
fn search_returns_one_line_hits_for_this_workspace() {
    let dir = TempDir::new("search");
    write_session(dir.path(), "in-ws", "/work/ws", "find me here");
    write_session(dir.path(), "other", "/work/other", "find me too");
    let index = sessions::open_index(dir.path()).unwrap().unwrap();
    let hits = sessions::search(&index, "/work/ws", "find me", 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].session_id, "in-ws");
    assert!(!hits[0].snippet.contains('\n'));
    assert!(hits[0].snippet.contains("find me here"));
}

#[test]
fn export_renders_this_workspaces_session() {
    let dir = TempDir::new("export");
    write_session(dir.path(), "in-ws", "/work/ws", "export me");
    let settings = ExportSettings::default();
    let markdown =
        sessions::export(dir.path(), "/work/ws", "in-ws", Format::Markdown, &settings).unwrap();
    assert!(markdown.contains("export me"), "{markdown}");
    let json = sessions::export(dir.path(), "/work/ws", "in-ws", Format::Json, &settings).unwrap();
    assert!(json.contains("\"session_id\""), "{json}");
}

#[test]
fn a_session_file_names_the_workspace_without_opening_it_for_resume() {
    let dir = TempDir::new("file-workspace");
    write_session(dir.path(), "in-ws", "/work/ws", "hello");
    assert_eq!(
        sessions::file_workspace(dir.path(), "in-ws").as_deref(),
        Some("/work/ws")
    );
    // An unknown id and a path-like id are the opener's to report.
    assert_eq!(sessions::file_workspace(dir.path(), "missing"), None);
    assert_eq!(sessions::file_workspace(dir.path(), "../x"), None);
    assert_eq!(
        sessions::other_workspace(dir.path(), "in-ws", "/work/ws"),
        None
    );
    let message = sessions::other_workspace(dir.path(), "in-ws", "/work/other")
        .expect("another workspace's session is refused");
    assert!(
        message.contains("in-ws")
            && message.contains("/work/ws")
            && message.contains("not in this workspace"),
        "{message}"
    );
    assert_eq!(
        sessions::other_workspace(dir.path(), "missing", "/work/ws"),
        None
    );
}

#[test]
fn export_refuses_another_workspace_and_a_path_like_id() {
    let dir = TempDir::new("export-bad");
    write_session(dir.path(), "in-ws", "/work/ws", "export me");
    let settings = ExportSettings::default();
    let error = sessions::export(
        dir.path(),
        "/work/other",
        "in-ws",
        Format::Markdown,
        &settings,
    )
    .unwrap_err();
    assert!(error.contains("not in this workspace"), "{error}");
    let error =
        sessions::export(dir.path(), "/work/ws", "../x", Format::Markdown, &settings).unwrap_err();
    assert!(error.contains("session id"), "{error}");
}

#[test]
fn export_redacts_known_secrets() {
    let dir = TempDir::new("redact");
    write_session(
        dir.path(),
        "in-ws",
        "/work/ws",
        "the key is SUPER-SECRET-KEY",
    );
    let settings = ExportSettings {
        secrets: vec!["SUPER-SECRET-KEY".to_owned()],
        ..ExportSettings::default()
    };
    let markdown =
        sessions::export(dir.path(), "/work/ws", "in-ws", Format::Markdown, &settings).unwrap();
    assert!(!markdown.contains("SUPER-SECRET-KEY"), "{markdown}");
    assert!(markdown.contains("<redacted>"), "{markdown}");
}
