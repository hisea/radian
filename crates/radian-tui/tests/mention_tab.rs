//! `Tab` completion of `@` file references (Phase 4). App-level: no process, a temp workspace.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_tui::app::App;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A temp directory with a unique name (no dev-dependency on a tempdir crate).
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-tab-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn tab() -> KeyEvent {
    key(KeyCode::Tab)
}

fn app_with(workspace: &Path, text: &str) -> App {
    let mut app = App::new("m", "s");
    app.set_workspace(workspace.to_path_buf());
    app.input = text.into();
    app
}

#[test]
fn a_unique_file_candidate_completes_and_adds_a_space() {
    let dir = TempDir::new("unique");
    dir.write("notes.txt", "x");
    let mut app = app_with(dir.path(), "see @not");
    assert_eq!(app.on_key(tab()), None);
    assert_eq!(app.input, "see @notes.txt ");
    assert!(app.completion.is_none());
}

#[test]
fn a_unique_directory_candidate_keeps_the_trailing_slash() {
    let dir = TempDir::new("dir");
    dir.write("sub/inner.txt", "x");
    let mut app = app_with(dir.path(), "@su");
    app.on_key(tab());
    assert_eq!(app.input, "@sub/");
    assert!(app.completion.is_none());
}

#[test]
fn several_candidates_complete_to_the_common_prefix_then_list() {
    let dir = TempDir::new("common");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    app.on_key(tab());
    assert_eq!(app.input, "@notes.", "the common prefix is inserted");
    assert!(app.completion.is_none(), "no list while the prefix grows");

    // A second Tab, already at the prefix, shows the list instead of changing the text.
    app.on_key(tab());
    assert_eq!(app.input, "@notes.");
    assert_eq!(
        app.completion.as_deref(),
        Some(["notes.md".to_string(), "notes.txt".to_string()].as_slice())
    );
}

#[test]
fn esc_closes_the_list_without_clearing_the_input() {
    let dir = TempDir::new("esc");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    app.on_key(tab());
    app.on_key(tab());
    assert!(app.completion.is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.completion.is_none());
    assert_eq!(
        app.input, "@notes.",
        "Esc must not clear the input while the list is up"
    );
}

#[test]
fn tab_on_a_word_that_is_not_a_reference_does_nothing() {
    let dir = TempDir::new("none-at");
    dir.write("notes.txt", "x");
    let mut app = app_with(dir.path(), "plain");
    app.on_key(tab());
    assert_eq!(app.input, "plain");
    assert!(app.completion.is_none());

    // An email is not a reference either.
    let mut app = app_with(dir.path(), "mail a@b.com");
    app.on_key(tab());
    assert_eq!(app.input, "mail a@b.com");
    assert!(app.completion.is_none());

    // A word that would match a file, but has no `@`, is still not a reference.
    let mut app = app_with(dir.path(), "notes");
    app.on_key(tab());
    assert_eq!(app.input, "notes");
    assert!(app.completion.is_none());
}

#[test]
fn no_candidate_leaves_the_text_alone() {
    let dir = TempDir::new("no-candidate");
    let mut app = app_with(dir.path(), "@nothing");
    app.on_key(tab());
    assert_eq!(app.input, "@nothing");
    assert!(app.completion.is_none());
}

#[test]
fn chinese_file_names_complete() {
    let dir = TempDir::new("cjk");
    dir.write("笔记.txt", "x");
    let mut app = app_with(dir.path(), "@笔");
    app.on_key(tab());
    assert_eq!(app.input, "@笔记.txt ");
}

#[test]
fn a_cursor_in_the_middle_replaces_only_the_prefix_before_it() {
    let dir = TempDir::new("middle");
    dir.write("notes.txt", "x");
    let mut app = app_with(dir.path(), "@noXYZ");
    // The cursor after `@no` (char index 3).
    app.input.line_start();
    for _ in 0..3 {
        app.input.right();
    }
    app.on_key(tab());
    assert_eq!(
        app.input, "@notes.txt XYZ",
        "only the text before the cursor is replaced"
    );
}

#[test]
fn typing_dismisses_the_list() {
    let dir = TempDir::new("typing");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    app.on_key(tab());
    app.on_key(tab());
    assert!(app.completion.is_some());
    app.on_key(key(KeyCode::Char('x')));
    assert!(app.completion.is_none());
}

#[test]
fn without_a_workspace_tab_does_nothing() {
    let mut app = App::new("m", "s");
    app.input = "@no".into();
    app.on_key(tab());
    assert_eq!(app.input, "@no");
    assert!(app.completion.is_none());
}
