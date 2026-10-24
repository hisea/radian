//! The arc TUI's leaving keys (ADR 0048 §1 and §13): `Esc` goes back exactly one level from
//! the innermost thing open — a float, an overlay view, the merge page, the task page, the
//! board, the plans page's task column — and never quits; `q` quits from everywhere but the
//! reject note, where it is a typed character.
//!
//! Every test walks up to the plans page's top level and presses `Esc` once more, so it also
//! pins the new top-level rule (a redraw with a `q quits` hint, never `Quit`) that the old
//! per-page handlers did not have. A test that stopped halfway would pass on the old code.
//!
//! The plans and tasks are real files in a scratch store (`Store::at`), so the pages read them
//! through the same `radian-arc` loaders the subcommands use.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};
use radian_tui::arc::{ArcApp, ArcKey, Focus, LogView, Page, board, render};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-back-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn store(dir: &Path) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(dir)
}

/// One confirmed plan: two `todo.ready` tasks, one `review.awaiting_human` task and one
/// `done.merged` task (the board's finished list needs a row), so every page and the merge
/// page apply, with more than one row to carry a selection.
struct Fixture {
    scratch: Scratch,
    store: Store,
    plan: String,
    ids: BTreeMap<&'static str, String>,
}

impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new("levels");
        let store = store(&scratch.path("repo"));
        let plan = store
            .add_plan("back", "the back plan", "a request\n")
            .unwrap();
        let mut ids = BTreeMap::new();
        for (name, state, phase, attempt) in [
            ("one", TaskState::Todo, "ready", 0),
            ("two", TaskState::Todo, "ready", 0),
            ("three", TaskState::Review, "awaiting_human", 1),
            ("four", TaskState::Done, "merged", 1),
        ] {
            let id = store
                .add_task(
                    &plan,
                    name,
                    &format!("the {name} task"),
                    Vec::new(),
                    false,
                    Vec::new(),
                    "the brief\n",
                )
                .unwrap();
            store
                .change_task(&id, |task| {
                    task.state = state;
                    task.phase = phase.to_owned();
                    task.attempt = attempt;
                    Ok(())
                })
                .unwrap();
            ids.insert(name, id);
        }
        store
            .change_plan(&plan, |plan| {
                plan.status = "confirmed".to_owned();
                plan.auto = "run".to_owned();
                Ok(())
            })
            .unwrap();
        Self {
            scratch,
            store,
            plan,
            ids,
        }
    }

    fn id(&self, name: &str) -> &str {
        self.ids.get(name).expect("the task exists")
    }

    /// The selected plan and the plans page.
    fn app(&self) -> ArcApp {
        let mut app =
            ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")));
        app.plan_sel = app
            .plans
            .iter()
            .position(|view| view.plan.item.id == self.plan)
            .expect("the plan is in the view");
        app
    }

    fn pos(&self, name: &str) -> usize {
        let app = self.app();
        let view = app.selected_plan().expect("the plan is selected");
        app.task_rows(view)
            .iter()
            .position(|row| row.id == self.id(name))
            .expect("the task is displayed")
    }

    /// The app on the merge page of the `review.awaiting_human` task, opened from its task
    /// page.
    fn merge(&self) -> ArcApp {
        let mut app = self.app();
        app.task_sel = self.pos("three");
        app.open_task();
        assert_eq!(app.on_key(&key(KeyCode::Char('m'))), ArcKey::Redraw);
        assert_eq!(app.page, Page::Merge);
        app
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl_c() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

fn render(app: &mut ArcApp, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| {
                    buffer
                        .cell((x, y))
                        .map(|cell| cell.symbol())
                        .unwrap_or(" ")
                        .to_string()
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn screen(app: &mut ArcApp, width: u16, height: u16) -> String {
    rows(&render(app, width, height)).join("\n")
}

/// The foreground of the cell at `(row, column)`.
fn fg(terminal: &Terminal<TestBackend>, row: u16, column: u16) -> Option<ratatui::style::Color> {
    terminal
        .backend()
        .buffer()
        .cell((column, row))
        .map(|cell| cell.fg)
}

/// The row and column of the first cell of `needle`.
fn fg_of(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
    let rows = rows(terminal);
    for (row, text) in rows.iter().enumerate() {
        if let Some(byte) = text.find(needle) {
            let column = text[..byte].chars().count();
            return (row as u16, column as u16);
        }
    }
    panic!("no row contains `{needle}`:\n{}", rows.join("\n"));
}

/// The last key: the top level, which only says how to leave and never quits.
fn assert_top_level(app: &mut ArcApp) {
    assert_eq!(app.page, Page::Plans, "the top level is the plans page");
    assert_eq!(app.focus, Focus::Plans, "the plan column has the focus");
    assert_eq!(
        app.on_key(&key(KeyCode::Esc)),
        ArcKey::Redraw,
        "Esc at the top level never quits"
    );
    let text = screen(app, 140, 30);
    assert!(
        text.contains("q quits"),
        "the footer says how to leave:\n{text}"
    );
    // And again: the top level stays the top level.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans);
}

// ---- the levels, one at a time -------------------------------------------------------

#[test]
fn esc_at_the_top_level_does_not_quit_and_says_q_quits() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    assert_eq!(app.focus, Focus::Plans, "the plan column is the top level");
    let plan_sel = app.plan_sel;

    assert_eq!(
        app.on_key(&key(KeyCode::Esc)),
        ArcKey::Redraw,
        "Esc on the plans page never quits"
    );
    assert_eq!(app.page, Page::Plans);
    assert_eq!(app.plan_sel, plan_sel, "nothing moves");

    let theme = app.theme.clone();
    let terminal = render(&mut app, 140, 30);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains("q quits"),
        "the footer says how to leave:\n{text}"
    );
    let (row, column) = fg_of(&terminal, "q quits");
    assert_eq!(
        fg(&terminal, row, column),
        theme.hint.fg,
        "the line is weak"
    );
    assert!(
        !text.contains("esc back"),
        "the top level has nowhere to go:\n{text}"
    );

    // The next key clears it.
    assert_eq!(app.on_key(&key(KeyCode::Down)), ArcKey::Redraw);
    let text = screen(&mut app, 140, 30);
    assert!(!text.contains("q quits"), "{text}");
}

#[test]
fn esc_moves_the_focus_from_the_task_column_to_the_plan_column() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let plan_sel = app.plan_sel;
    app.focus = Focus::Tasks;
    app.task_sel = 1;

    let text = screen(&mut app, 170, 30);
    assert!(
        text.contains("esc back"),
        "the task column has somewhere to go:\n{text}"
    );

    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.focus, Focus::Plans, "Esc leaves the task column");
    assert_eq!(app.plan_sel, plan_sel, "the plan selection is kept");
    assert_eq!(app.task_sel, 1, "the task selection is kept");
    assert_eq!(app.page, Page::Plans);
    assert!(
        !screen(&mut app, 140, 30).contains("q quits"),
        "one Esc changes one level"
    );

    let text = screen(&mut app, 170, 30);
    assert!(
        !text.contains("esc back"),
        "the plan column shows no esc back:\n{text}"
    );

    assert_top_level(&mut app);
}

#[test]
fn esc_walks_back_from_the_merge_command_log_to_the_top_level() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.focus = Focus::Tasks;
    app.task_sel = fixture.pos("three");
    let plan_sel = app.plan_sel;
    let task_sel = app.task_sel;
    let task = fixture.id("three").to_owned();
    app.open_task();
    assert_eq!(app.page, Page::Task);

    assert_eq!(app.on_key(&key(KeyCode::Char('m'))), ArcKey::Redraw);
    assert_eq!(app.page, Page::Merge);
    if let Some(page) = app.merge.as_mut() {
        page.log = Some(LogView::new("cargo test", "the command printed this\n"));
    }

    // Level 2: the command log.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(
        app.merge.as_ref().is_some_and(|page| page.log.is_none()),
        "Esc closes the command log"
    );
    assert_eq!(app.page, Page::Merge, "and nothing under it");

    // Level 3: the merge page, back to the task page it was opened from.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Task);
    assert_eq!(app.task.as_ref().map(|page| page.id.clone()), Some(task));
    assert!(app.merge.is_none());

    // Level 4: the task page, back to the plans page with its selections kept.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans);
    assert_eq!(app.focus, Focus::Tasks);
    assert_eq!(app.plan_sel, plan_sel, "the plan selection is kept");
    assert_eq!(app.task_sel, task_sel, "the task selection is kept");

    // Level 6: the task column, back to the plan column.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.focus, Focus::Plans);
    assert_eq!(app.plan_sel, plan_sel);
    assert_eq!(app.task_sel, task_sel);

    // Level 7: the top level.
    assert_top_level(&mut app);
}

#[test]
fn esc_from_the_merge_page_returns_to_the_board_and_then_to_the_plans_page() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    assert_eq!(app.on_key(&key(KeyCode::Char('b'))), ArcKey::Redraw);
    app.board.column = board::NEEDS_YOU;
    app.board.row = 0;
    let selected = app.board.selected().map(|card| card.id.clone());
    assert!(selected.is_some(), "a card is selected");

    assert_eq!(app.on_key(&key(KeyCode::Char('M'))), ArcKey::Redraw);
    assert_eq!(app.page, Page::Merge);
    if let Some(page) = app.merge.as_mut() {
        page.log = Some(LogView::new("cargo test", "the command printed this\n"));
    }

    // The log over the merge page, then the merge page back to the board.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Merge);
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Board, "the merge page returns to the board");
    assert_eq!(
        app.board.selected().map(|card| card.id.clone()),
        selected,
        "the board selection is kept"
    );

    // The board, back to the plans page.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans);

    assert_top_level(&mut app);
}

// ---- the overlay views ---------------------------------------------------------------

#[test]
fn esc_walks_back_from_the_loop_log_over_the_finished_list_to_the_top_level() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('b')));
    app.on_key(&key(KeyCode::Char('d')));
    assert!(app.board.finished, "the finished list is open");
    app.on_key(&key(KeyCode::Char('L')));
    assert!(app.log_open, "the loop's log is on top");

    // The loop's log covers every page.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(!app.log_open, "Esc closes the log");
    assert!(app.board.finished, "and nothing under it");
    assert_eq!(app.page, Page::Board);

    // The finished list covers the board's columns.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(!app.board.finished, "the next Esc closes the finished list");
    assert_eq!(app.page, Page::Board);

    // The board goes back to the plans page.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans, "and the next one leaves the board");

    assert_top_level(&mut app);
}

#[test]
fn esc_from_a_task_opened_in_the_finished_list_keeps_the_finished_list() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('b')));
    app.on_key(&key(KeyCode::Char('d')));
    assert!(app.board.finished, "the finished list is open");
    let row = app.board.selected_done().map(|row| row.id.clone());
    assert_eq!(row.as_deref(), Some(fixture.id("four")));

    assert_eq!(app.on_key(&key(KeyCode::Enter)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Task);

    // The finished list is hidden behind the task page, so it is not a level yet: Esc
    // returns to the board with the list still open.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Board, "the task page returns to the board");
    assert!(
        app.board.finished,
        "the hidden finished list is untouched, not consumed"
    );
    assert_eq!(
        app.board.selected_done().map(|row| row.id.clone()),
        row,
        "the finished selection is kept"
    );

    // Now it is visible, so it is the next level.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(!app.board.finished, "the visible list closes");
    assert_eq!(app.page, Page::Board);

    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans);

    assert_top_level(&mut app);
}

#[test]
fn esc_from_the_board_does_not_consume_a_hidden_merge_log() {
    let fixture = Fixture::new();
    let mut app = fixture.merge();
    if let Some(page) = app.merge.as_mut() {
        page.log = Some(LogView::new("cargo test", "the command printed this\n"));
    }

    // `b` leaves the merge page, and its log, behind.
    assert_eq!(app.on_key(&key(KeyCode::Char('b'))), ArcKey::Redraw);
    assert_eq!(app.page, Page::Board);

    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(
        app.page,
        Page::Plans,
        "Esc leaves the board instead of closing the invisible log"
    );
    assert!(
        app.merge.as_ref().is_some_and(|page| page.log.is_some()),
        "the merge page keeps its log for when it is visible again"
    );

    assert_top_level(&mut app);
}

// ---- floats --------------------------------------------------------------------------

#[test]
fn esc_closes_a_confirm_float_and_then_the_levels_under_it() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.focus = Focus::Tasks;
    app.task_sel = 0;
    let one = fixture.id("one").to_owned();
    let plan_sel = app.plan_sel;
    assert_eq!(app.on_key(&key(KeyCode::Char('x'))), ArcKey::Redraw);
    assert!(
        matches!(app.float, Some(radian_tui::arc::Float::Confirm { .. })),
        "{:?}",
        app.float
    );

    // Level 1 only: the float closes, the page and the focus stay.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(app.float.is_none(), "Esc closes the float");
    assert_eq!(app.page, Page::Plans, "and only the float");
    assert_eq!(app.focus, Focus::Tasks, "the focus is unchanged");
    assert_eq!(app.take_action(), None, "nothing was queued");
    assert_eq!(
        fixture.store.load_task(&one).unwrap().item.phase,
        "ready",
        "the task is unchanged"
    );

    // Then the task column, and the top level: never `Quit`.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.focus, Focus::Plans);
    assert_eq!(app.plan_sel, plan_sel);
    assert_top_level(&mut app);

    // Ctrl-C still quits from a float, whatever Esc does.
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('x')));
    assert!(app.float.is_some());
    assert_eq!(app.on_key(&ctrl_c()), ArcKey::Quit);
}

// ---- q -------------------------------------------------------------------------------

#[test]
fn q_quits_from_a_float_and_every_page() {
    let fixture = Fixture::new();

    let mut app = fixture.app();
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    let mut app = fixture.app();
    app.task_sel = fixture.pos("two");
    app.open_task();
    assert_eq!(app.page, Page::Task);
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    let mut app = fixture.merge();
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('b')));
    assert_eq!(app.page, Page::Board);
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('b')));
    app.on_key(&key(KeyCode::Char('d')));
    assert!(app.board.finished);
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('L')));
    assert!(app.log_open);
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);

    // A float takes every other key first, but `q` still leaves; only the reject note below
    // types it.
    let mut app = fixture.app();
    app.focus = Focus::Tasks;
    app.task_sel = 0;
    assert_eq!(app.on_key(&key(KeyCode::Char('x'))), ArcKey::Redraw);
    assert!(matches!(
        app.float,
        Some(radian_tui::arc::Float::Confirm { .. })
    ));
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);
}

#[test]
fn q_in_the_reject_note_is_typed_then_esc_walks_out() {
    let fixture = Fixture::new();
    let mut app = fixture.merge();
    assert_eq!(app.on_key(&key(KeyCode::Char('r'))), ArcKey::Redraw);
    for ch in "quiet".chars() {
        assert_eq!(app.on_key(&key(KeyCode::Char(ch))), ArcKey::Redraw);
    }
    assert_eq!(
        app.float,
        Some(radian_tui::arc::Float::Reject {
            note: "quiet".to_owned()
        }),
        "the `q` is a letter of the note"
    );
    assert_eq!(app.page, Page::Merge, "the TUI stays open");

    // Esc closes the note float and nothing under it.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert!(app.float.is_none());
    assert_eq!(app.page, Page::Merge);

    // Then the merge page, the task page and the plans page, one level each.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Task);
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Plans);
    assert_top_level(&mut app);

    // Ctrl-C quits from the note, where `q` is a character.
    let mut app = fixture.merge();
    assert_eq!(app.on_key(&key(KeyCode::Char('r'))), ArcKey::Redraw);
    assert!(matches!(
        app.float,
        Some(radian_tui::arc::Float::Reject { .. })
    ));
    assert_eq!(app.on_key(&ctrl_c()), ArcKey::Quit);
}
