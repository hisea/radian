//! `Tab` completion of `@` file references (Phase 4). App-level: no process, a temp workspace.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_tui::app::{App, Effect, Input};
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

/// Presses Tab the way the event loop does: the effect reads the directory, and its result
/// goes back into `update`. Panics if Tab asks for anything else — it must never send.
fn tab_complete(app: &mut App, workspace: &Path) {
    let effects = app.on_key(tab()).effects;
    assert_eq!(effects.len(), 1, "{effects:?}");
    match &effects[0] {
        Effect::CompleteMention { text, cursor } => {
            let completion = radian_tui::mention::complete(text, *cursor, workspace);
            app.update(Input::CompletionReady(completion));
        }
        other => panic!("Tab must only ask for a completion, got {other:?}"),
    }
}

fn app_with(workspace: &Path, text: &str) -> App {
    let mut app = App::new("m", "s");
    app.set_workspace(workspace.to_path_buf());
    app.input = text.into();
    app
}

#[test]
fn arc_worktrees_are_not_offered_but_the_rest_of_dot_radian_is() {
    let dir = TempDir::new("arc-worktrees");
    dir.write(".radian/arc/worktrees/t1/notes.txt", "x");
    dir.write(".radian/arc/plans/PL0001-a.md", "x");
    let text = "@.radian/arc/";
    let completion =
        radian_tui::mention::complete(text, text.chars().count(), dir.path()).expect("a reference");
    assert_eq!(completion.candidates, [".radian/arc/plans/"]);
}

#[test]
fn arc_gates_are_not_offered_but_the_rest_of_dot_radian_is() {
    let dir = TempDir::new("arc-gates");
    dir.write(".radian/arc/gates/commands/notes.txt", "x");
    dir.write(".radian/arc/plans/PL0001-a.md", "x");
    let text = "@.radian/arc/";
    let completion =
        radian_tui::mention::complete(text, text.chars().count(), dir.path()).expect("a reference");
    assert_eq!(completion.candidates, [".radian/arc/plans/"]);
}

#[test]
fn a_unique_file_candidate_completes_and_adds_a_space() {
    let dir = TempDir::new("unique");
    dir.write("notes.txt", "x");
    let mut app = app_with(dir.path(), "see @not");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "see @notes.txt ");
    assert!(app.completion().is_none());
}

#[test]
fn a_unique_directory_candidate_keeps_the_trailing_slash() {
    let dir = TempDir::new("dir");
    dir.write("sub/inner.txt", "x");
    let mut app = app_with(dir.path(), "@su");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "@sub/");
    assert!(app.completion().is_none());
}

#[test]
fn several_candidates_complete_to_the_common_prefix_then_list() {
    let dir = TempDir::new("common");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "@notes.", "the common prefix is inserted");
    assert!(app.completion().is_none(), "no list while the prefix grows");

    // A second Tab, already at the prefix, shows the list instead of changing the text.
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "@notes.");
    let inserts: Vec<&str> = app
        .completion()
        .expect("the list")
        .iter()
        .map(|candidate| candidate.insert.as_str())
        .collect();
    assert_eq!(inserts, ["notes.md", "notes.txt"]);
}

#[test]
fn esc_closes_the_list_without_clearing_the_input() {
    let dir = TempDir::new("esc");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    assert!(app.completion().is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.completion().is_none());
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
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "plain");
    assert!(app.completion().is_none());

    // An email is not a reference either.
    let mut app = app_with(dir.path(), "mail a@b.com");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "mail a@b.com");
    assert!(app.completion().is_none());

    // A word that would match a file, but has no `@`, is still not a reference.
    let mut app = app_with(dir.path(), "notes");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "notes");
    assert!(app.completion().is_none());
}

#[test]
fn no_candidate_leaves_the_text_alone() {
    let dir = TempDir::new("no-candidate");
    let mut app = app_with(dir.path(), "@nothing");
    tab_complete(&mut app, dir.path());
    assert_eq!(app.input, "@nothing");
    assert!(app.completion().is_none());
}

#[test]
fn chinese_file_names_complete() {
    let dir = TempDir::new("cjk");
    dir.write("笔记.txt", "x");
    let mut app = app_with(dir.path(), "@笔");
    tab_complete(&mut app, dir.path());
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
    tab_complete(&mut app, dir.path());
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
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    assert!(app.completion().is_some());
    app.on_key(key(KeyCode::Char('x')));
    assert!(app.completion().is_none());
}

/// A key with an action of its own dismisses the stale list too (not just typing). Up and
/// Down now belong to the menu (see the tests below), so this uses another bound key.
#[test]
fn a_bound_key_dismisses_the_list_too() {
    let dir = TempDir::new("bound-key");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    assert!(app.completion().is_some());
    app.on_key(key(KeyCode::Home));
    assert!(
        app.completion().is_none(),
        "the list was computed for a word the key may have changed"
    );
}

/// The menu opens on its first (best-ranked) row, and Up/Down move the highlight and wrap.
#[test]
fn the_menu_opens_on_the_first_row_and_up_down_wrap() {
    let dir = TempDir::new("menu-wrap");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    assert_eq!(app.completion_cursor, 0, "the first row is highlighted");
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.completion_cursor, 1);
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.completion_cursor, 0, "down wraps to the first row");
    app.on_key(key(KeyCode::Up));
    assert_eq!(app.completion_cursor, 1, "up wraps to the last row");
}

/// Enter accepts the highlighted row: it inserts the candidate and never sends the line.
#[test]
fn enter_accepts_the_highlighted_candidate_without_sending() {
    let dir = TempDir::new("menu-enter");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    // The second row (`notes.txt`) is highlighted.
    app.on_key(key(KeyCode::Down));
    let result = app.on_key(key(KeyCode::Enter));
    assert!(result.effects.is_empty(), "Enter must not send: {result:?}");
    assert_eq!(app.input, "@notes.txt ");
    assert!(app.completion().is_none());
}

/// Tab accepts the highlighted row too (it is not a "recompute" once the menu is up).
#[test]
fn tab_accepts_the_highlighted_candidate() {
    let dir = TempDir::new("menu-tab");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    app.on_key(key(KeyCode::Down));
    let result = app.on_key(tab());
    assert!(result.effects.is_empty(), "{result:?}");
    assert_eq!(app.input, "@notes.txt ");
    assert!(app.completion().is_none());
}

/// Esc closes the menu and keeps the input untouched.
#[test]
fn escape_closes_the_menu_and_keeps_the_input() {
    let dir = TempDir::new("menu-esc");
    dir.write("notes.txt", "x");
    dir.write("notes.md", "x");
    let mut app = app_with(dir.path(), "@no");
    tab_complete(&mut app, dir.path());
    tab_complete(&mut app, dir.path());
    assert!(app.completion().is_some());
    let result = app.on_key(key(KeyCode::Esc));
    assert!(result.effects.is_empty(), "{result:?}");
    assert!(app.completion().is_none());
    // The second Tab-complete filled the common prefix (`notes.`); Esc keeps it.
    assert_eq!(
        app.input, "@notes.",
        "Esc closes the menu, it does not clear the line"
    );
}

#[test]
fn without_a_workspace_tab_does_nothing() {
    let mut app = App::new("m", "s");
    app.input = "@no".into();
    let result = app.on_key(tab());
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.input, "@no");
    assert!(app.completion().is_none());
}
