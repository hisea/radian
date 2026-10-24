//! The arc TUI's plans and task pages (ADR 0048 §1, §13): the dependency tree, the state
//! marks and their roles, the tree/list toggle, the attempt verdicts, the missing journal, and
//! the diff tab's file list for a merged and an unmerged task.
//!
//! The plans and tasks are real files in a scratch store (`Store::at`), so the page reads them
//! through the same `radian-arc` loaders the subcommands use. The diff tests use a real git
//! repository; the merged task's merge commit is a real `--no-ff` merge, which is what decides
//! that the file list is the task's own change and not `main..head`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::gate::{Stage, Verdict, verdict_path};
use radian_arc::read;
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};
use radian_tui::arc::render;
use radian_tui::arc::{ArcApp, ArcKey, Focus, PlanFilter, TaskTab, actions};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

/// A temp directory that removes itself.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-arc-tui-{label}-{}-{n}", std::process::id()));
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

/// A store in `dir`: arc's `config.toml` and the layout it expects.
fn store(dir: &Path) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config::default_text("main")).unwrap();
    Store::at(dir)
}

fn add_plan(store: &Store, slug: &str, title: &str) -> String {
    store.add_plan(slug, title, "a request\n").unwrap()
}

#[allow(clippy::too_many_arguments)]
fn add_task(
    store: &Store,
    plan: &str,
    slug: &str,
    after: &[&str],
    state: TaskState,
    phase: &str,
    attempt: u32,
    cost: Option<f64>,
) -> String {
    let id = store
        .add_task(
            plan,
            slug,
            &format!("the {slug} task"),
            after.iter().map(|id| (*id).to_owned()).collect(),
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
            task.cost_usd = cost;
            Ok(())
        })
        .unwrap();
    id
}

/// The plan every page test selects: one merged task, a dependency tree with a branch, a task
/// with two parents, a failed task, a waiting task, a doing task and a verifying one.
struct Fixture {
    scratch: Scratch,
    store: Store,
    plan: String,
    ids: BTreeMap<&'static str, String>,
}

impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new("pages");
        let store = store(&scratch.path("repo"));
        // A finished plan (the green ✓ in the list).
        let done = add_plan(&store, "first", "the first plan");
        add_task(
            &store,
            &done,
            "done",
            &[],
            TaskState::Done,
            "merged",
            1,
            Some(0.12),
        );
        // A draft plan.
        let draft = add_plan(&store, "images", "the image plan");
        add_task(
            &store,
            &draft,
            "img-a",
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
        add_task(
            &store,
            &draft,
            "img-b",
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );

        // The selected plan.
        let plan = add_plan(&store, "retro", "arc improvements from the first plan");
        let mut ids = BTreeMap::new();
        let a = add_task(
            &store,
            &plan,
            "a",
            &[],
            TaskState::Done,
            "merged",
            1,
            Some(0.08),
        );
        let b = add_task(
            &store,
            &plan,
            "b",
            &[],
            TaskState::Done,
            "merged",
            1,
            Some(0.11),
        );
        let c = add_task(
            &store,
            &plan,
            "c",
            &[&a],
            TaskState::Done,
            "merged",
            1,
            Some(0.14),
        );
        let e = add_task(
            &store,
            &plan,
            "e",
            &[&a],
            TaskState::Review,
            "awaiting_human",
            2,
            Some(0.23),
        );
        // Two parents: the first draws the tree edge, the second is `also after`; `e` is not
        // merged, so this todo task also `waits` on it.
        let d = add_task(
            &store,
            &plan,
            "d",
            &[&a, &e],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
        let failed = add_task(
            &store,
            &plan,
            "f",
            &[],
            TaskState::Failed,
            "exhausted",
            3,
            Some(0.05),
        );
        let doing = add_task(
            &store,
            &plan,
            "g",
            &[],
            TaskState::Doing,
            "running",
            1,
            None,
        );
        let verifying = add_task(
            &store,
            &plan,
            "h",
            &[],
            TaskState::Review,
            "verifying",
            1,
            None,
        );
        for (name, id) in [
            ("a", &a),
            ("b", &b),
            ("c", &c),
            ("d", &d),
            ("e", &e),
            ("f", &failed),
            ("g", &doing),
            ("h", &verifying),
        ] {
            ids.insert(name, id.clone());
        }
        store
            .change_plan(&plan, |plan| {
                plan.status = "confirmed".to_owned();
                plan.auto = "merge".to_owned();
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

    fn pos(&self, name: &str) -> usize {
        let app = self.app();
        let view = app.selected_plan().expect("the plan is selected");
        app.task_rows(view)
            .iter()
            .position(|row| row.id == self.id(name))
            .expect("the task is displayed")
    }

    /// Selects the fixture plan and opens the app on it.
    fn app(&self) -> ArcApp {
        let mut app =
            ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")));
        let index = app
            .plans
            .iter()
            .position(|view| view.plan.item.id == self.plan)
            .unwrap();
        app.plan_sel = index;
        app
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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

/// The foreground of the first cell of `needle` in its row.
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

/// The row and column of the `mark` that sits before the first `needle` in its own column.
fn mark_cell(terminal: &Terminal<TestBackend>, needle: &str, mark: char) -> (u16, u16) {
    let rows = rows(terminal);
    for (row, text) in rows.iter().enumerate() {
        let Some(byte) = text.find(needle) else {
            continue;
        };
        let id_column = text[..byte].chars().count();
        let chars: Vec<char> = text.chars().collect();
        if let Some(column) = chars[..id_column].iter().rposition(|ch| *ch == mark) {
            return (row as u16, column as u16);
        }
    }
    panic!("`{mark}` is not before `{needle}`:\n{}", rows.join("\n"));
}

#[test]
fn the_plans_page_draws_the_list_and_the_selected_plan() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    // `all` shows the finished plan too: this test is about the list and the frame, not the
    // default filter.
    app.on_key(&key(KeyCode::Char('f')));
    let text = screen(&mut app, 120, 30);
    for needle in [
        "arc".to_owned(),
        "plans".to_owned(),
        "task".to_owned(),
        "merge".to_owned(),
        "main —".to_owned(),
        "loop ◻ stopped".to_owned(),
        "PLANS".to_owned(),
        "PL0001-first".to_owned(),
        "off   · 1/1 · $0.12".to_owned(),
        "PL0002-images".to_owned(),
        "off   · 0/2 ·     —".to_owned(),
        format!(
            "{} confirmed · P3 · auto merge · 3/8 merged · $0.61",
            fixture.plan
        ),
        "arc improvements from the first plan".to_owned(),
        fixture.id("a").to_owned(),
        "done.merged".to_owned(),
        "#1".to_owned(),
        "$0.08".to_owned(),
        "↑↓ select".to_owned(),
        "q quit".to_owned(),
    ] {
        assert!(text.contains(&needle), "missing `{needle}`:\n{text}");
    }
}

#[test]
fn the_plan_summary_starts_under_the_id() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.on_key(&key(KeyCode::Char('f')));
    let text = screen(&mut app, 120, 30);
    let summary = text
        .lines()
        .find(|line| line.contains("merge · 3/8 · $0.61"))
        .unwrap_or_else(|| panic!("no plan summary:\n{text}"));
    assert!(
        summary.starts_with("         merge · 3/8 · $0.61"),
        "the summary starts under the id, past the mark and the priority: {summary:?}"
    );
}

#[test]
fn the_hints_keep_their_old_labels_and_drop_when_narrow() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let wide = screen(&mut app, 160, 30);
    for needle in [
        "↑↓ select   enter open task",
        "g graph/list",
        "f filter",
        "tab pages",
        "q quit",
    ] {
        assert!(wide.contains(needle), "missing `{needle}`:\n{wide}");
    }

    // At 120 the row cannot hold every hint. The view keys the header already names go first,
    // and no label is shortened to make room.
    let narrow = screen(&mut app, 120, 30);
    for needle in ["enter open task", "g graph/list", "+/- priority", "q quit"] {
        assert!(narrow.contains(needle), "missing `{needle}`:\n{narrow}");
    }
    for dropped in ["f filter", "tab pages", "enter task", "g list", "f view"] {
        assert!(
            !narrow.contains(dropped),
            "unexpected `{dropped}`:\n{narrow}"
        );
    }
}

#[test]
fn the_plan_list_scrolls_to_keep_the_selected_plan_visible() {
    let scratch = Scratch::new("plan-scroll");
    let store = store(&scratch.path("repo"));
    let ids: Vec<String> = (1..=12)
        .map(|n| add_plan(&store, &format!("plan-{n:02}"), &format!("plan {n}")))
        .collect();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    // Five plans fit under the heading at this height; the rest are below the window.
    let text = screen(&mut app, 120, 20);
    assert!(text.contains(&ids[0]), "{text}");
    assert!(!text.contains(&ids[11]), "{text}");

    for _ in 0..11 {
        assert_eq!(app.on_key(&key(KeyCode::Down)), ArcKey::Redraw);
    }
    let terminal = render(&mut app, 120, 20);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains(&ids[11]),
        "the selected plan's row is drawn:\n{text}"
    );
    assert!(
        !text.contains(&ids[0]),
        "the list scrolled past the first plan:\n{text}"
    );
    assert!(
        rows(&terminal)[4].contains(&ids[7]),
        "the list starts at the plan that keeps the selection visible:\n{text}"
    );

    for _ in 0..11 {
        assert_eq!(app.on_key(&key(KeyCode::Up)), ArcKey::Redraw);
    }
    let terminal = render(&mut app, 120, 20);
    let text = rows(&terminal).join("\n");
    assert!(text.contains(&ids[0]), "{text}");
    assert!(
        rows(&terminal)[4].contains(&ids[0]),
        "selecting the first plan draws the top of the list:\n{text}"
    );
    assert!(!text.contains(&ids[11]), "{text}");
}

/// The five plan rows that fill a 20-row area exactly (16 body rows = the heading, the blank
/// under it, five plans and the four separators between them).
#[test]
fn the_plan_list_holds_every_plan_that_fits_the_area_exactly() {
    let scratch = Scratch::new("plan-fill");
    let store = store(&scratch.path("repo"));
    let ids: Vec<String> = (1..=5)
        .map(|n| add_plan(&store, &format!("plan-{n:02}"), &format!("plan {n}")))
        .collect();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    for _ in 0..4 {
        assert_eq!(app.on_key(&key(KeyCode::Down)), ArcKey::Redraw);
    }
    let text = screen(&mut app, 120, 20);
    assert!(text.contains(&ids[4]), "the last plan is visible:\n{text}");
    assert!(
        text.contains(&ids[0]),
        "the list did not scroll away the first plan, because all five fit:\n{text}"
    );
}

#[test]
fn the_default_plans_view_hides_finished_and_cancelled_plans() {
    let fixture = Fixture::new();
    fixture
        .store
        .change_plan("PL0002-images", |plan| {
            plan.status = "cancelled".to_owned();
            plan.auto = "off".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = fixture.app();
    assert_eq!(
        app.filter,
        PlanFilter::Active,
        "every start opens on active"
    );
    let text = screen(&mut app, 140, 30);
    assert!(text.contains(&fixture.plan), "{text}");
    assert!(
        !text.contains("PL0001-first"),
        "a finished plan is hidden from active:\n{text}"
    );
    assert!(
        !text.contains("PL0002-images"),
        "a cancelled plan is hidden from active:\n{text}"
    );
    assert!(
        text.contains("active · 2 hidden"),
        "the header names the view and counts what it hides:\n{text}"
    );
}

#[test]
fn f_cycles_the_plan_views_in_order_and_back() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    assert_eq!(app.filter, PlanFilter::Active);
    for expected in [
        PlanFilter::All,
        PlanFilter::Done,
        PlanFilter::Cancelled,
        PlanFilter::Active,
    ] {
        assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
        assert_eq!(app.filter, expected, "the `f` cycle order");
    }
    // A second cycle starts over.
    assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
    assert_eq!(app.filter, PlanFilter::All);
}

#[test]
fn the_selection_stays_on_a_plan_in_both_views_and_moves_to_the_first_otherwise() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let selected = app.selected_plan().unwrap().plan.item.id.clone();
    // `all` holds the selected plan: the selection stays on it.
    assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
    assert_eq!(app.filter, PlanFilter::All);
    assert_eq!(
        app.selected_plan().unwrap().plan.item.id,
        selected,
        "the selection stays on a plan that the new view holds"
    );
    // `done` holds only the finished plan: the selection moves to its first plan.
    assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
    assert_eq!(app.filter, PlanFilter::Done);
    assert_eq!(
        app.selected_plan().unwrap().plan.item.id,
        "PL0001-first",
        "the selection moves to the first plan of a view without the old one"
    );
}

#[test]
fn an_empty_plan_view_shows_one_weak_line_and_an_empty_frame() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    for _ in 0..3 {
        app.on_key(&key(KeyCode::Char('f')));
    }
    assert_eq!(app.filter, PlanFilter::Cancelled);
    let terminal = render(&mut app, 140, 30);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains("no cancelled plans"),
        "the empty view says so:\n{text}"
    );
    assert!(!text.contains('╭'), "the right frame is empty:\n{text}");
    let (row, column) = fg_of(&terminal, "no cancelled plans");
    assert_eq!(
        fg(&terminal, row, column),
        radian_tui::theme::Theme::default().hint.fg,
        "the empty-view line is weak"
    );
}

#[test]
fn a_refresh_that_finishes_the_selected_plan_moves_the_selection() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let selected = app.selected_plan().unwrap().plan.item.id.clone();
    app.task_sel = 3;
    let tasks: Vec<String> = fixture
        .store
        .plans()
        .unwrap()
        .into_iter()
        .find(|view| view.plan.item.id == selected)
        .unwrap()
        .tasks
        .iter()
        .map(|task| task.item.id.clone())
        .collect();
    for task in tasks {
        fixture
            .store
            .change_task(&task, |task| {
                task.state = TaskState::Done;
                task.phase = "merged".to_owned();
                Ok(())
            })
            .unwrap();
    }
    app.refresh();
    // The selected plan left `active`; the first plan still in it is the draft one.
    assert_eq!(
        app.selected_plan().unwrap().plan.item.id,
        "PL0002-images",
        "the selection follows a refresh that empties its plan out of the view"
    );
    assert_eq!(app.task_sel, 0, "the task selection resets with the plan");
    assert_eq!(app.hidden, 2, "two finished plans are hidden now");
}

#[test]
fn all_shows_no_hidden_count() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let text = screen(&mut app, 140, 30);
    assert!(
        text.contains("active · 1 hidden"),
        "active hides the finished plan:\n{text}"
    );
    assert_eq!(app.on_key(&key(KeyCode::Char('f'))), ArcKey::Redraw);
    let text = screen(&mut app, 140, 30);
    assert!(text.contains("all"), "{text}");
    assert!(
        !text.contains("hidden"),
        "`all` hides nothing and shows no count:\n{text}"
    );
}

#[test]
fn the_task_list_scrolls_to_keep_the_selected_task_visible() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    // Five task rows fit in the frame at this height; the last task is below the window.
    app.on_key(&key(KeyCode::Right));
    for _ in 0..7 {
        assert_eq!(app.on_key(&key(KeyCode::Down)), ArcKey::Redraw);
    }
    let view = app.selected_plan().unwrap();
    let shown = app.task_rows(view);
    let first = shown[0].id.clone();
    let last = shown[7].id.clone();
    let text = screen(&mut app, 120, 14);
    assert!(text.contains(&last), "the selected task is drawn:\n{text}");
    assert!(!text.contains(&first), "the list scrolled:\n{text}");

    for _ in 0..7 {
        assert_eq!(app.on_key(&key(KeyCode::Up)), ArcKey::Redraw);
    }
    let text = screen(&mut app, 120, 14);
    assert!(text.contains(&first), "{text}");
    assert!(
        !text.contains(&last),
        "the list returns to the top:\n{text}"
    );
}

#[test]
fn the_attempts_list_scrolls_to_keep_the_selected_attempt_visible() {
    let fixture = Fixture::new();
    let task = fixture.id("e").to_owned();
    for attempt in 1..=10 {
        write_verdicts(&fixture, &task, attempt, &[("commands", true, None)]);
    }
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    app.on_key(&key(KeyCode::Char('l')));
    app.on_key(&key(KeyCode::Char('l')));
    // Three attempt rows fit; the latest attempt is selected and drawn at the bottom.
    let text = screen(&mut app, 120, 12);
    let attempt_rows: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("✓ commands"))
        .collect();
    assert_eq!(attempt_rows.len(), 3, "{text}");
    assert!(
        attempt_rows.iter().any(|line| line.contains("attempt 10")),
        "{text}"
    );
    assert!(
        !attempt_rows
            .iter()
            .any(|line| line.contains("attempt 1   ")),
        "{text}"
    );

    for _ in 0..9 {
        app.on_key(&key(KeyCode::Char('[')));
    }
    let text = screen(&mut app, 120, 12);
    let attempt_rows: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("✓ commands"))
        .collect();
    assert!(
        attempt_rows
            .iter()
            .any(|line| line.contains("attempt 1   ")),
        "{text}"
    );
    assert!(
        !attempt_rows.iter().any(|line| line.contains("attempt 10")),
        "{text}"
    );
}

#[test]
fn the_attempts_window_moves_only_when_the_selection_leaves_it() {
    let fixture = Fixture::new();
    let task = fixture.id("e").to_owned();
    for attempt in 1..=10 {
        write_verdicts(&fixture, &task, attempt, &[("commands", true, None)]);
    }
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    app.on_key(&key(KeyCode::Char('l')));
    app.on_key(&key(KeyCode::Char('l')));
    // The latest attempt is selected; the window shows attempts 8, 9 and 10.
    let _ = screen(&mut app, 120, 12);
    // Two moves up leave the selection inside the same window: attempts 8, 9 and 10 stay.
    app.on_key(&key(KeyCode::Char('[')));
    app.on_key(&key(KeyCode::Char('[')));
    let text = screen(&mut app, 120, 12);
    let attempt_rows: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("✓ commands"))
        .collect();
    assert!(
        attempt_rows.iter().any(|line| line.contains("attempt 10")),
        "the window did not move while the selection stayed inside it:\n{text}"
    );
    assert!(
        !attempt_rows.iter().any(|line| line.contains("attempt 7")),
        "the window did not pin the selection to its bottom row either:\n{text}"
    );
}

#[test]
fn a_cancelled_plan_is_marked_and_named_cancelled() {
    let fixture = Fixture::new();
    fixture
        .store
        .change_plan("PL0002-images", |plan| {
            plan.status = "cancelled".to_owned();
            plan.auto = "off".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = fixture.app();
    // The cancelled plan is hidden from the default view; `all` shows it with its mark.
    app.on_key(&key(KeyCode::Char('f')));
    let theme = app.theme.clone();
    let terminal = render(&mut app, 120, 30);
    // A cancelled plan is not a confirmed one: it takes the cancelled mark.
    let (row, column) = mark_cell(&terminal, "PL0002-images", '✕');
    assert_eq!(fg(&terminal, row, column), theme.plan_cancelled.fg);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains("off   · 0/2 ·     —"),
        "the list names the columns without a status prefix:\n{text}"
    );
    assert!(
        !text.contains("cancelled · "),
        "the mark says cancelled, not a prefix:\n{text}"
    );
    // The rest of a cancelled plan's row is weak (ADR 0048 decision 11).
    let (row, _) = fg_of(&terminal, "PL0002-images");
    let summary = rows(&terminal)[row as usize + 1].clone();
    assert_eq!(
        fg(&terminal, row + 1, 4),
        theme.hint.fg,
        "the summary row is weak: {summary}"
    );
}

#[test]
fn the_tree_places_a_branch_and_names_the_other_parent() {
    let fixture = Fixture::new();
    let app = fixture.app();
    let view = app.selected_plan().expect("the plan is selected");
    let tree = app.task_rows(view);
    // Every task once (mutation: a two-parent task drawn twice).
    let ids: Vec<&str> = tree.iter().map(|row| row.id.as_str()).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(ids.len(), unique.len(), "a task is drawn once: {ids:?}");

    let c = tree.iter().find(|row| row.id == fixture.id("c")).unwrap();
    let d = tree.iter().find(|row| row.id == fixture.id("d")).unwrap();
    let e = tree.iter().find(|row| row.id == fixture.id("e")).unwrap();
    assert_eq!(c.connector, "├─", "the first child branches");
    assert_eq!(
        d.connector, "└─",
        "the last child closes the branch: {tree:?}"
    );
    assert_eq!(e.connector, "├─", "{tree:?}");
    assert_eq!(d.also_after, vec![fixture.id("e").to_owned()]);
    assert_eq!(d.waits, vec![fixture.id("e").to_owned()]);
    assert!(c.also_after.is_empty());
    assert!(c.waits.is_empty(), "a merged parent is not waited on");

    // The same rows on screen, connectors and all. A wide page leaves room for the parents.
    let mut app = fixture.app();
    let text = screen(&mut app, 160, 30);
    assert!(text.contains("├─"), "a branch connector is drawn:\n{text}");
    assert!(
        text.contains("└─"),
        "the last child connector is drawn:\n{text}"
    );
    assert!(
        text.contains(&format!("also after {}", fixture.id("e"))),
        "the second parent is named:\n{text}"
    );
    assert!(
        text.contains(&format!("waits {}", fixture.id("e"))),
        "the unmerged parent is named:\n{text}"
    );
}

#[test]
fn every_state_mark_takes_its_role() {
    let fixture = Fixture::new();
    // A finished *confirmed* plan draws the done mark: a draft plan's mark is `◌` whatever its
    // tasks reached (the draft rule comes first).
    fixture
        .store
        .change_plan("PL0001-first", |plan| {
            plan.status = "confirmed".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = fixture.app();
    // The finished plan is hidden from the default view; `all` shows every mark.
    app.on_key(&key(KeyCode::Char('f')));
    let theme = app.theme.clone();
    let terminal = render(&mut app, 120, 30);

    let check = |needle: &str, mark: char, role: ratatui::style::Style, what: &str| {
        let (row, column) = mark_cell(&terminal, needle, mark);
        assert_eq!(
            fg(&terminal, row, column),
            role.fg,
            "{what}: {needle} {mark}"
        );
    };
    check(fixture.id("a"), '✓', theme.status_ok, "done");
    check(fixture.id("f"), '×', theme.status_error, "failed");
    check(fixture.id("g"), '●', theme.model, "doing");
    check(fixture.id("h"), '●', theme.model, "verifying");
    check(fixture.id("e"), '!', theme.notice_label, "waiting");
    check(fixture.id("d"), '◻', theme.hint, "todo");
    // The plan list's own marks: ✓ all merged (green), ◉ a task needs the owner, ◌ draft
    // (weak).
    check("PL0001-first", '✓', theme.plan_done, "a finished plan");
    check(
        &fixture.plan,
        '◉',
        theme.plan_needs_you,
        "a plan whose task waits for you",
    );
    check("PL0002-images", '◌', theme.plan_draft, "a draft plan");
}

#[test]
fn the_plans_page_shows_each_priority_in_its_role() {
    use radian_arc::store::model::PLAN_PRIORITIES;

    let scratch = Scratch::new("priority");
    let store = store(&scratch.path("repo"));
    let mut plans = Vec::new();
    for (index, priority) in PLAN_PRIORITIES.iter().enumerate() {
        let id = add_plan(
            &store,
            &format!("p{index}"),
            &format!("the {priority} plan"),
        );
        add_task(
            &store,
            &id,
            &format!("t{index}"),
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
        radian_arc::store::plan_ops::set_priority(&store, &id, priority).unwrap();
        plans.push(((*priority).to_owned(), id));
    }
    let mut app = ArcApp::with_session_dir(store.clone(), Some(scratch.path("sessions")));
    let theme = app.theme.clone();
    let role = |priority: &str| match priority {
        "P0" => theme.priority_p0.fg,
        "P1" => theme.priority_p1.fg,
        "P2" => theme.priority_p2.fg,
        "P3" => theme.priority_p3.fg,
        "P4" => theme.priority_p4.fg,
        _ => theme.priority_p5.fg,
    };

    // The left list puts the priority before the id, in that priority's role.
    let terminal = render(&mut app, 140, 40);
    for (priority, id) in &plans {
        let needle = format!("{priority} {id}");
        let (row, column) = fg_of(&terminal, &needle);
        assert_eq!(
            fg(&terminal, row, column),
            role(priority),
            "the list row: {needle}"
        );
    }
    // The frame label puts it after the status, in the same role, for every selected plan.
    for (index, (priority, id)) in plans.iter().enumerate() {
        app.plan_sel = index;
        let terminal = render(&mut app, 140, 40);
        let text = rows(&terminal).join("\n");
        assert!(
            text.contains(&format!("{id} draft · {priority} · auto off")),
            "the frame label: {priority}\n{text}"
        );
        let (row, column) = fg_of(&terminal, &format!("{priority} ·"));
        assert_eq!(
            fg(&terminal, row, column),
            role(priority),
            "{priority} label"
        );
    }

    // `+` on the P2 plan moves it to P1; the next refresh shows P1 in both places.
    app.plan_sel = 2;
    app.on_key(&key(KeyCode::Char('+')));
    let action = app.take_action().expect("the step is queued");
    assert_eq!(
        action,
        actions::Action::Priority {
            plan: plans[2].1.clone(),
            level: "P1".to_owned(),
        }
    );
    let outcome = actions::run(&action, &store, &actions::Library);
    assert!(outcome.ok, "{outcome:?}");
    app.finish_action(outcome);
    let terminal = render(&mut app, 140, 40);
    let id = &plans[2].1;
    let (row, column) = fg_of(&terminal, &format!("P1 {id}"));
    assert_eq!(
        fg(&terminal, row, column),
        theme.priority_p1.fg,
        "the list row shows the new priority"
    );
    let (row, column) = fg_of(&terminal, "P1 ·");
    assert_eq!(
        fg(&terminal, row, column),
        theme.priority_p1.fg,
        "the frame label shows the new priority"
    );
}

#[test]
fn g_switches_between_the_tree_and_the_flat_list() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    assert!(!app.flat);
    assert_eq!(app.on_key(&key(KeyCode::Char('g'))), ArcKey::Redraw);
    assert!(app.flat);
    let view = app.selected_plan().unwrap();
    let rows = app.task_rows(view);
    let d = rows.iter().find(|row| row.id == fixture.id("d")).unwrap();
    assert!(d.connector.is_empty(), "the flat list has no connectors");
    assert_eq!(
        d.after,
        vec![fixture.id("a").to_owned(), fixture.id("e").to_owned()]
    );
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains(&format!("after {}, {}", fixture.id("a"), fixture.id("e"))),
        "{text}"
    );
    // And back.
    app.on_key(&key(KeyCode::Char('g')));
    assert!(!app.flat);
}

#[test]
fn the_header_reports_the_remote_main_and_the_loop_lock() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    let lock_path = fixture.store.root().join(radian_arc::store::RUNNER_LOCK);
    // No lock file: stopped, and the probe creates nothing.
    assert!(!radian_tui::arc::loop_running(&fixture.store));
    assert!(!lock_path.exists(), "the probe never creates the lock file");

    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    {
        use fs2::FileExt;
        lock.lock_exclusive().unwrap();
    }
    assert!(radian_tui::arc::loop_running(&fixture.store));
    app.refresh();
    app.apply_main(Some("8a3ec2ba".to_owned()));
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("main 8a3ec2ba"), "{text}");
    assert!(text.contains("loop ● running"), "{text}");
    {
        fs2::FileExt::unlock(&lock).unwrap();
    }
    app.refresh();
    assert!(screen(&mut app, 120, 30).contains("loop ◻ stopped"));
}

#[test]
fn the_task_selection_follows_the_drawn_order() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    // Tree order: a, c, e, d, b, f, g, h. Right focuses the task list; Down selects c.
    app.on_key(&key(KeyCode::Right));
    assert_eq!(app.focus, Focus::Tasks);
    app.on_key(&key(KeyCode::Down));
    let view = app.selected_plan().unwrap();
    assert_eq!(app.task_rows(view)[1].id, fixture.id("c"));
    assert_eq!(app.selected_task().unwrap().item.id, fixture.id("c"));
    app.on_key(&key(KeyCode::Enter));
    assert_eq!(app.task.as_ref().unwrap().id, fixture.id("c"));

    // Flat order is plan order: a, b, c, e, d, f, g, h; the second row is b.
    app.close_task();
    app.flat = true;
    app.task_sel = 0;
    app.on_key(&key(KeyCode::Down));
    let view = app.selected_plan().unwrap();
    assert_eq!(app.task_rows(view)[1].id, fixture.id("b"));
    app.on_key(&key(KeyCode::Enter));
    assert_eq!(app.task.as_ref().unwrap().id, fixture.id("b"));

    // `g` keeps the same task selected when the order changes.
    app.close_task();
    app.flat = false;
    app.task_sel = 1; // c in the tree
    assert_eq!(app.selected_task().unwrap().item.id, fixture.id("c"));
    app.on_key(&key(KeyCode::Char('g')));
    assert!(app.flat);
    assert_eq!(app.selected_task().unwrap().item.id, fixture.id("c"));
}

#[test]
fn the_selected_task_row_shows_the_cursor_only_with_focus() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = 1; // c in the tree
    let text = screen(&mut app, 120, 30);
    let row = text
        .lines()
        .find(|line| line.contains(fixture.id("c")))
        .expect("the c row is drawn");
    // The plan list is on the left of the divider; the task row is on the right. The cursor
    // on the left (the selected plan) says nothing about the task list's focus.
    let right = row.split_once('│').map(|(_, right)| right).unwrap_or("");
    assert!(!right.contains('❯'), "no cursor without focus: {row}");

    app.focus = Focus::Tasks;
    let text = screen(&mut app, 120, 30);
    let row = text
        .lines()
        .find(|line| line.contains(fixture.id("c")))
        .expect("the c row is drawn");
    let right = row.split_once('│').map(|(_, right)| right).unwrap_or("");
    assert!(
        right.contains("❯ "),
        "the cursor is on the selected row: {row}"
    );
}

#[test]
fn a_stale_diff_reply_does_not_block_the_next_attempt() {
    use radian_arc::read::TaskDiff;
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    assert_eq!(app.task.as_ref().unwrap().attempt, Some(2));
    app.on_key(&key(KeyCode::Char('l')));
    let latest = app
        .diff_request()
        .expect("the latest attempt asks for its diff");
    assert_eq!(latest.1, 2);
    app.mark_diff_loading(latest.clone());
    assert_eq!(
        app.diff_request(),
        None,
        "the same request is not started twice"
    );

    // The attempt changes while attempt 2's diff is in flight: attempt 1 asks at once.
    app.on_key(&key(KeyCode::Char('[')));
    assert_eq!(app.task.as_ref().unwrap().attempt, Some(1));
    let next = app.diff_request().expect("the new attempt asks at once");
    assert_eq!(next.1, 1);
    assert!(
        !app.apply_diff(&latest.0, latest.1, Ok(TaskDiff::default())),
        "the old attempt's reply is stale"
    );
    assert_eq!(
        app.diff_request(),
        Some(next.clone()),
        "a stale reply never blocks the new request"
    );
    app.mark_diff_loading(next.clone());
    assert!(app.apply_diff(&next.0, next.1, Ok(TaskDiff::default())));
    assert_eq!(app.diff_request(), None);
}

#[test]
fn follow_then_k_scrolls_the_diff_up_from_the_end() {
    use radian_arc::read::{DiffFile, TaskDiff};
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    app.on_key(&key(KeyCode::Char('l')));
    let id = app.task.as_ref().unwrap().id.clone();
    let attempt = app.task.as_ref().unwrap().attempt.unwrap();
    let text: String = (1..=40).map(|i| format!("+line {i}\n")).collect();
    let file = DiffFile {
        path: "big.txt".to_owned(),
        added: 40,
        removed: 0,
        text,
    };
    app.apply_diff(&id, attempt, Ok(TaskDiff { files: vec![file] }));

    app.on_key(&key(KeyCode::Char('f')));
    assert_eq!(
        app.task.as_ref().unwrap().diff_scroll,
        39,
        "f goes to the last body row"
    );
    let text = screen(&mut app, 120, 40);
    let first = first_diff_body_row(&text);
    assert!(first.contains("line 40"), "{first}");

    app.on_key(&key(KeyCode::Char('k')));
    assert_eq!(
        app.task.as_ref().unwrap().diff_scroll,
        38,
        "k moves up one line"
    );
    let text = screen(&mut app, 120, 40);
    let first = first_diff_body_row(&text);
    assert!(first.contains("line 39"), "{first}");

    app.on_key(&key(KeyCode::Char('j')));
    app.on_key(&key(KeyCode::Char('j')));
    assert_eq!(
        app.task.as_ref().unwrap().diff_scroll,
        39,
        "j stops at the end"
    );
}

/// The first body row under the diff frame titled with `big.txt`.
fn first_diff_body_row(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let title = lines
        .iter()
        .position(|line| line.contains("╭─ big.txt"))
        .expect("the diff frame is drawn");
    lines[title + 1].to_string()
}

#[test]
fn a_long_file_list_still_leaves_the_diff_a_body_row() {
    use radian_arc::read::{DiffFile, TaskDiff};
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    app.on_key(&key(KeyCode::Char('l')));
    let id = app.task.as_ref().unwrap().id.clone();
    let attempt = app.task.as_ref().unwrap().attempt.unwrap();
    let files = (1..=20)
        .map(|i| DiffFile {
            path: format!("file-{i:02}.txt"),
            added: 1,
            removed: 0,
            text: format!("+the change of file-{i:02}.txt\n"),
        })
        .collect();
    app.apply_diff(&id, attempt, Ok(TaskDiff { files }));

    let text = screen(&mut app, 80, 24);
    assert!(
        text.contains("╭─ file-01.txt"),
        "the selected file's frame is drawn:\n{text}"
    );
    assert!(
        text.contains("the change of file-01.txt"),
        "the frame keeps at least one body row:\n{text}"
    );
}

#[test]
fn the_task_page_has_the_frame_the_tabs_and_the_attempt_selector() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    assert_eq!(app.on_key(&key(KeyCode::Enter)), ArcKey::Redraw);
    assert_eq!(app.page, radian_tui::arc::Page::Task);
    let text = screen(&mut app, 120, 30);
    let id = fixture.id("e");
    for needle in [
        &format!(
            "{id} review.awaiting_human · attempt 2 · $0.23 · plan {}",
            fixture.plan
        ),
        "the e task",
        "brief",
        "conversation",
        "diff",
        "attempts",
        "attempt 1  2",
        "no session journal yet",
    ] {
        assert!(text.contains(needle), "missing `{needle}`:\n{text}");
    }
    // Esc goes back to the plans page; Tab on it reopens the task page.
    assert_eq!(app.on_key(&key(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, radian_tui::arc::Page::Plans);
    assert!(screen(&mut app, 120, 30).contains("PLANS"));
    app.on_key(&key(KeyCode::Tab));
    assert!(screen(&mut app, 120, 30).contains("conversation"));
    // `q` leaves from either page.
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);
}

/// The open task page's tab.
fn tab(app: &ArcApp) -> TaskTab {
    app.task.as_ref().expect("the task page is open").tab
}

#[test]
fn the_task_tabs_cycle_in_order_in_both_directions() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("d");
    app.open_task();
    assert_eq!(
        tab(&app),
        TaskTab::Brief,
        "a task without an attempt opens here"
    );
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains("brief  ·  conversation  ·  diff  ·  attempts"),
        "the tabs are drawn in cycle order:\n{text}"
    );
    for expected in [
        TaskTab::Conversation,
        TaskTab::Diff,
        TaskTab::Attempts,
        TaskTab::Brief,
    ] {
        assert_eq!(app.on_key(&key(KeyCode::Tab)), ArcKey::Redraw);
        assert_eq!(tab(&app), expected, "`Tab` cycles forward");
    }
    let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
    for expected in [
        TaskTab::Attempts,
        TaskTab::Diff,
        TaskTab::Conversation,
        TaskTab::Brief,
    ] {
        assert_eq!(app.on_key(&back), ArcKey::Redraw);
        assert_eq!(tab(&app), expected, "`Shift-Tab` cycles back");
    }
    // A terminal that sends `Tab` with the shift modifier cycles back too.
    let shifted = KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT);
    assert_eq!(app.on_key(&shifted), ArcKey::Redraw);
    assert_eq!(tab(&app), TaskTab::Attempts);
}

#[test]
fn a_task_page_opens_on_the_brief_until_the_task_has_an_attempt() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("d");
    app.open_task();
    assert_eq!(
        tab(&app),
        TaskTab::Brief,
        "an `attempt: 0` task opens on its brief"
    );
    app.close_task();
    app.task_sel = fixture.pos("e");
    app.open_task();
    assert_eq!(
        tab(&app),
        TaskTab::Conversation,
        "a task with an attempt opens on its conversation"
    );
}

#[test]
fn the_brief_tab_shows_the_task_body_and_no_frontmatter() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("d");
    app.open_task();
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("the brief"), "the body is drawn:\n{text}");
    for key in [
        "schema_version",
        "allow_noop",
        "extra_stages",
        "cost_usd",
        "head_sha",
    ] {
        assert!(
            !text.contains(key),
            "the frontmatter key `{key}` is drawn:\n{text}"
        );
    }
}

#[test]
fn a_brief_shows_its_rework_sections() {
    let fixture = Fixture::new();
    let id = fixture.id("e").to_owned();
    fixture
        .store
        .change_task(&id, |task| {
            task.body.push_str("\n## Rework 1\n\ntighten the tests\n");
            Ok(())
        })
        .unwrap();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    // A task with an attempt opens on the conversation; the brief is the tab before it.
    app.on_key(&key(KeyCode::Char('h')));
    assert_eq!(tab(&app), TaskTab::Brief);
    let text = screen(&mut app, 120, 40);
    assert!(text.contains("the brief"), "{text}");
    assert!(
        text.contains("Rework 1"),
        "the rework heading is drawn:\n{text}"
    );
    assert!(text.contains("tighten the tests"), "{text}");
}

#[test]
fn a_rework_added_later_shows_at_the_next_refresh() {
    let fixture = Fixture::new();
    let id = fixture.id("e").to_owned();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    app.on_key(&key(KeyCode::Char('h')));
    let text = screen(&mut app, 120, 40);
    assert!(!text.contains("Rework"), "{text}");

    radian_arc::store::plan_ops::add_rework(&fixture.store, &id, "closed the gap").unwrap();
    app.refresh();
    assert_eq!(
        tab(&app),
        TaskTab::Brief,
        "a tab the owner picked stays picked"
    );
    let text = screen(&mut app, 120, 40);
    assert!(
        text.contains("Rework 3"),
        "the next refresh shows the appended section (the task is at attempt 2):\n{text}"
    );
    assert!(text.contains("closed the gap"), "{text}");
}

#[test]
fn a_brief_heading_is_drawn_in_the_transcript_heading_role() {
    let fixture = Fixture::new();
    let id = fixture.id("d").to_owned();
    fixture
        .store
        .change_task(&id, |task| {
            task.body = "## The ask\n\nthe explanation\n".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("d");
    app.open_task();
    let theme = app.theme.clone();
    let terminal = render(&mut app, 120, 30);
    let text = rows(&terminal).join("\n");
    assert!(text.contains("The ask"), "{text}");
    assert!(!text.contains("##"), "no `#` is drawn:\n{text}");
    let (row, column) = fg_of(&terminal, "The ask");
    assert_eq!(
        fg(&terminal, row, column),
        theme.md_heading.fg,
        "the heading role's color"
    );
    let cell = terminal
        .backend()
        .buffer()
        .cell((column, row))
        .expect("the heading's first cell");
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::BOLD),
        "the heading role is bold: {cell:?}"
    );
}

/// The run directories of `task`: one verdict file per stage of each attempt.
fn write_verdicts(
    fixture: &Fixture,
    task: &str,
    attempt: u32,
    verdicts: &[(&str, bool, Option<&str>)],
) {
    let label = format!("{task}-{attempt}");
    let stage = |name: &str| match name {
        "commands" => Stage::Commands,
        "mutation" => Stage::Mutation,
        "review" => Stage::Review,
        "ci" => Stage::Ci,
        "human" => Stage::Human,
        "integration" => Stage::Integration,
        other => panic!("unknown stage {other}"),
    };
    for (name, pass, reason) in verdicts {
        let mut verdict = Verdict::human(
            "c".repeat(40),
            *pass,
            reason.map(|reason| reason.to_owned()),
        );
        verdict.stage = stage(name);
        if *name == "integration" {
            verdict.candidate_sha = Some("d".repeat(40));
            verdict.base_sha = Some("a".repeat(40));
        }
        let path = verdict_path(fixture.store.repo(), task, &label, verdict.stage).unwrap();
        verdict.write_new(&path).expect("a verdict is written once");
    }
}

#[test]
fn the_attempts_tab_lists_each_verdict_with_its_reason() {
    let fixture = Fixture::new();
    let task = fixture.id("e");
    write_verdicts(
        &fixture,
        task,
        1,
        &[("commands", false, Some("command_failed"))],
    );
    write_verdicts(
        &fixture,
        task,
        2,
        &[
            ("commands", true, None),
            ("human", true, None),
            ("integration", false, Some("conflict")),
        ],
    );
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    assert_eq!(app.task.as_ref().unwrap().attempts.len(), 2);
    // The tab row, then the attempts tab (`l` twice).
    app.on_key(&key(KeyCode::Char('l')));
    app.on_key(&key(KeyCode::Char('l')));
    let text = screen(&mut app, 120, 30);
    for needle in [
        "attempt 1",
        "attempt 2",
        "× commands (command_failed)",
        "✓ commands",
        "✓ human",
        "× integration (conflict)",
    ] {
        assert!(text.contains(needle), "missing `{needle}`:\n{text}");
    }
    // `[` selects the previous attempt, and its own verdicts are shown.
    app.on_key(&key(KeyCode::Char('[')));
    assert_eq!(app.task.as_ref().unwrap().attempt, Some(1));
    assert_eq!(app.on_key(&key(KeyCode::Char(']'))), ArcKey::Redraw);
    assert_eq!(app.task.as_ref().unwrap().attempt, Some(2));
}

#[test]
fn the_conversation_tab_renders_the_selected_attempts_worker_journal() {
    let fixture = Fixture::new();
    let task = fixture.id("e").to_owned();
    let sessions = fixture.scratch.path("sessions");
    fs::create_dir_all(&sessions).unwrap();
    write_verdicts(
        &fixture,
        &task,
        1,
        &[("commands", false, Some("command_failed"))],
    );
    write_verdicts(&fixture, &task, 2, &[("commands", true, None)]);
    // The default `fresh` worker: attempt 2's session id is `<task>-2`, and its journal is
    // named by the id its `execution.json` records.
    let session = format!("{task}-2");
    write_execution(&fixture, &task, 2, &session);
    write_journal(
        &sessions,
        &session,
        "the worker said hello",
        "and the assistant replied",
    );
    // Attempt 1 wrote no record: its journal is looked for under the task id.
    write_journal(&sessions, &task, "the first attempt", "the first reply");
    let mut app = ArcApp::with_session_dir(fixture.store.clone(), Some(sessions));
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == fixture.plan)
        .unwrap();
    app.task_sel = fixture.pos("e");
    app.open_task();
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("the worker said hello"), "{text}");
    assert!(text.contains("and the assistant replied"), "{text}");
    // The attach view's own user marker (the assistant body is the bare text column).
    assert!(text.contains("❯"), "the user marker:\n{text}");
    // `[` selects attempt 1 and its own journal; attempt 2's is not kept on screen.
    app.on_key(&key(KeyCode::Char('[')));
    assert_eq!(app.task.as_ref().unwrap().attempt, Some(1));
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("the first attempt"), "{text}");
    assert!(
        !text.contains("the worker said hello"),
        "attempt 2's journal is not drawn for attempt 1:\n{text}"
    );
}

/// A `doing.running` task has no `execution.json` yet: the claim's `start.json` names the
/// session, so the Conversation tab opens `<task>-<attempt>.jsonl` and follows it while the
/// worker appends. Mutation: the tab falls back to the task id even when a session id is known.
#[test]
fn the_conversation_tab_follows_a_running_attempts_journal() {
    let fixture = Fixture::new();
    let task = fixture.id("g").to_owned();
    let sessions = fixture.scratch.path("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let session = format!("{task}-1");
    write_start(&fixture, &task, 1, &session);
    write_journal(&sessions, &session, "the running worker", "is writing");
    let mut app = ArcApp::with_session_dir(fixture.store.clone(), Some(sessions.clone()));
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == fixture.plan)
        .unwrap();
    app.task_sel = fixture.pos("g");
    app.open_task();
    app.on_key(&key(KeyCode::Char('f')));
    app.apply_pending_scroll();
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("the running worker"), "{text}");
    assert!(text.contains("is writing"), "{text}");
    assert!(!text.contains("no session journal yet"), "{text}");

    // The worker writes another line; the refresh polls the same journal and the followed
    // view shows it.
    append_journal(&sessions, &session, "and one more line");
    app.refresh();
    app.apply_pending_scroll();
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("and one more line"), "{text}");
}

/// The task frame's state alone does not say whether a `doing` worker is alive (ADR 0048
/// decisions 7.9 and 9): the row adds the heartbeat's age, `stalled?` past the stall window,
/// `starting` before the first beat, and `loop stopped` when nothing holds `runner.lock`.
#[test]
fn a_doing_tasks_row_reports_its_worker_heartbeat_and_a_stopped_loop() {
    let scratch = Scratch::new("freshness");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "live", "the live plan");
    let fresh = add_task(
        &store,
        &plan,
        "fresh",
        &[],
        TaskState::Doing,
        "running",
        1,
        None,
    );
    let old = add_task(
        &store,
        &plan,
        "old",
        &[],
        TaskState::Doing,
        "running",
        2,
        None,
    );
    add_task(
        &store,
        &plan,
        "none",
        &[],
        TaskState::Doing,
        "running",
        1,
        None,
    );
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            Ok(())
        })
        .unwrap();
    // A beat from just now, and one far past the default 900-second stall window.
    write_heartbeat(&store, &fresh, 1, &radian_arc::store::plan_ops::utc_now());
    write_heartbeat(&store, &old, 2, "2000-01-01T00:00:00Z");

    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.plan_sel = app
        .plans
        .iter()
        .position(|view| view.plan.item.id == plan)
        .unwrap();
    let theme = app.theme.clone();
    let terminal = render(&mut app, 160, 30);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains("heartbeat 0s ago") || text.contains("heartbeat 1s ago"),
        "a fresh beat shows its age:\n{text}"
    );
    assert!(text.contains("starting"), "no beat yet:\n{text}");
    assert!(text.contains("stalled?"), "an old beat:\n{text}");
    assert_eq!(
        text.matches("loop stopped").count(),
        3,
        "every doing row names the stopped loop:\n{text}"
    );
    // The stale beat takes the warning style.
    let (row, column) = fg_of(&terminal, "stalled?");
    assert_eq!(fg(&terminal, row, column), theme.status_warn.fg);

    // A held `runner.lock` is a loop that can advance the state: no row says it stopped.
    let lock_path = app.store.root().join(radian_arc::store::RUNNER_LOCK);
    let lock = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    {
        use fs2::FileExt;
        lock.lock_exclusive().unwrap();
    }
    app.refresh();
    let text = screen(&mut app, 160, 30);
    assert!(!text.contains("loop stopped"), "{text}");
    {
        fs2::FileExt::unlock(&lock).unwrap();
    }
}

/// The attempt's `execution.json`, where the wrapper records the worker's session id.
fn write_execution(fixture: &Fixture, task: &str, attempt: u32, session: &str) {
    let dir = radian_arc::store::attempt_ops::run_dir(&fixture.store, task, attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("execution.json"),
        json!({ "session_id": session }).to_string(),
    )
    .unwrap();
}

/// The claim's `start.json`, which names the session before the wrapper writes its own
/// records.
fn write_start(fixture: &Fixture, task: &str, attempt: u32, session: &str) {
    let dir = radian_arc::store::attempt_ops::attempt_dir(&fixture.store, task, attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(radian_arc::store::attempt_ops::START_FILE),
        json!({ "session_id": session }).to_string(),
    )
    .unwrap();
}

/// The wrapper's heartbeat, with the given last beat.
fn write_heartbeat(store: &Store, task: &str, attempt: u32, last_beat: &str) {
    let dir = radian_arc::store::attempt_ops::attempt_dir(store, task, attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(radian_arc::store::attempt_ops::HEARTBEAT_FILE),
        json!({ "last_beat": last_beat }).to_string(),
    )
    .unwrap();
}

/// Appends one assistant line to an existing worker journal (the worker still writing).
fn append_journal(dir: &Path, id: &str, assistant: &str) {
    use radian_core::TranscriptJournal;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    runtime.block_on(async {
        let journal = radian_session::SessionJournal::resume(&path, id)
            .expect("the journal resumes for the next line");
        journal
            .append(radian_core::JournalRecord::Message {
                index: 2,
                message: radian_core::Message::assistant(assistant),
            })
            .await
            .unwrap();
    });
}

/// A worker journal: one user message and one assistant reply.
fn write_journal(dir: &Path, id: &str, user: &str, assistant: &str) {
    use radian_core::TranscriptJournal;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    runtime.block_on(async {
        let journal = radian_session::SessionJournal::create(
            &path,
            id,
            radian_session::SessionCreatedData::new(dir.to_string_lossy().into_owned(), json!({})),
        )
        .expect("the journal is created");
        journal
            .append(radian_core::JournalRecord::Message {
                index: 0,
                message: radian_core::Message::user(user),
            })
            .await
            .unwrap();
        journal
            .append(radian_core::JournalRecord::Message {
                index: 1,
                message: radian_core::Message::assistant(assistant),
            })
            .await
            .unwrap();
    });
}

// ---- the diff tab: a real repository -------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "arc test")
        .env("GIT_AUTHOR_EMAIL", "arc@example.com")
        .env("GIT_COMMITTER_NAME", "arc test")
        .env("GIT_COMMITTER_EMAIL", "arc@example.com")
        .args(["-c", "core.hooksPath="])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A repository with one merged task, one fast-forwarded task and one unmerged task.
struct GitFixture {
    scratch: Scratch,
    repo: PathBuf,
    store: Store,
    merged: String,
    fast: String,
    unmerged: String,
}

impl GitFixture {
    fn new() -> Self {
        let scratch = Scratch::new("diff");
        let repo = scratch.path("repo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        write(&repo, "base.txt", "base\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);

        let store = store(&repo);
        let plan = add_plan(&store, "diff", "diff plan");
        let merged = add_task(
            &store,
            &plan,
            "merged",
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
        let unmerged = add_task(
            &store,
            &plan,
            "unmerged",
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );

        // The merged task's branch: one new file.
        git(
            &repo,
            &["checkout", "-q", "-B", &format!("work/{merged}"), &base],
        );
        write(&repo, "task.txt", "the task change\n");
        git(&repo, &["add", "task.txt"]);
        git(&repo, &["commit", "-q", "-m", "the task change"]);
        let merged_head = git(&repo, &["rev-parse", "HEAD"]);

        // The unmerged task's branch: a different new file.
        git(
            &repo,
            &["checkout", "-q", "-B", &format!("work/{unmerged}"), &base],
        );
        write(&repo, "other.txt", "the other change\nanother line\n");
        git(&repo, &["add", "other.txt"]);
        git(&repo, &["commit", "-q", "-m", "the other change"]);
        let unmerged_head = git(&repo, &["rev-parse", "HEAD"]);

        // Main moves on its own, then merges the task branch with a real merge commit.
        git(&repo, &["checkout", "-q", "main"]);
        write(&repo, "main.txt", "main only\n");
        git(&repo, &["add", "main.txt"]);
        git(&repo, &["commit", "-q", "-m", "main only"]);
        git(
            &repo,
            &[
                "merge",
                "-q",
                "--no-ff",
                &format!("work/{merged}"),
                "-m",
                "merge",
            ],
        );
        let merge_commit = git(&repo, &["rev-parse", "HEAD"]);
        git(
            &repo,
            &[
                "update-ref",
                "refs/remotes/origin/main",
                &git(&repo, &["rev-parse", "main"]),
            ],
        );

        // A task merged by fast-forwarding main: its branch has two commits, and main had not
        // moved since the branch's base. The first-parent diff of the head would show only
        // the second commit.
        let fast = add_task(
            &store,
            &plan,
            "fast",
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
        git(
            &repo,
            &[
                "checkout",
                "-q",
                "-B",
                &format!("work/{fast}"),
                &merge_commit,
            ],
        );
        write(&repo, "first.txt", "the first commit\n");
        git(&repo, &["add", "first.txt"]);
        git(&repo, &["commit", "-q", "-m", "the first commit"]);
        write(&repo, "second.txt", "the second commit\n");
        git(&repo, &["add", "second.txt"]);
        git(&repo, &["commit", "-q", "-m", "the second commit"]);
        let fast_head = git(&repo, &["rev-parse", "HEAD"]);
        // Main had not moved since the branch's base, so the merge fast-forwards it to the
        // branch's head; the remote main moves with it.
        git(&repo, &["checkout", "-q", "main"]);
        git(
            &repo,
            &["merge", "-q", "--ff-only", &format!("work/{fast}")],
        );
        git(
            &repo,
            &["update-ref", "refs/remotes/origin/main", &fast_head],
        );

        store
            .change_task(&merged, |task| {
                task.state = TaskState::Done;
                task.phase = "merged".to_owned();
                task.attempt = 1;
                task.base_sha = Some(base.clone());
                task.head_sha = Some(merged_head.clone());
                Ok(())
            })
            .unwrap();
        store
            .change_task(&unmerged, |task| {
                task.state = TaskState::Doing;
                task.phase = "running".to_owned();
                task.attempt = 1;
                task.base_sha = Some(base.clone());
                task.head_sha = Some(unmerged_head.clone());
                Ok(())
            })
            .unwrap();
        store
            .change_task(&fast, |task| {
                task.state = TaskState::Done;
                task.phase = "merged".to_owned();
                task.attempt = 1;
                task.base_sha = Some(merge_commit.clone());
                task.head_sha = Some(fast_head.clone());
                Ok(())
            })
            .unwrap();
        let mut integration = Verdict::human(merged_head.clone(), true, None);
        integration.stage = Stage::Integration;
        integration.candidate_sha = Some(merge_commit.clone());
        integration.base_sha = Some(base.clone());
        let label = format!("{merged}-1");
        integration
            .write_new(&verdict_path(&repo, &merged, &label, Stage::Integration).unwrap())
            .unwrap();
        // The fast-forward's candidate is the task head; main was moved to it.
        let mut integration = Verdict::human(fast_head.clone(), true, None);
        integration.stage = Stage::Integration;
        integration.candidate_sha = Some(fast_head);
        integration.base_sha = Some(merge_commit);
        let label = format!("{fast}-1");
        integration
            .write_new(&verdict_path(&repo, &fast, &label, Stage::Integration).unwrap())
            .unwrap();

        Self {
            scratch,
            repo,
            store,
            merged,
            fast,
            unmerged,
        }
    }

    fn app(&self) -> ArcApp {
        let mut app =
            ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")));
        let plan = self.store.plans().unwrap()[0].plan.item.id.clone();
        app.plan_sel = app
            .plans
            .iter()
            .position(|view| view.plan.item.id == plan)
            .unwrap();
        app
    }

    fn open_diff(&self, name: &str) -> (ArcApp, String, u32) {
        let mut app = self.app();
        let view = self
            .store
            .plan_view(&app.plans[app.plan_sel].plan.item.id)
            .unwrap();
        let target = match name {
            "merged" => &self.merged,
            "fastforward" => &self.fast,
            _ => &self.unmerged,
        };
        app.task_sel = view
            .tasks
            .iter()
            .position(|task| task.item.id == *target)
            .unwrap();
        app.open_task();
        // To the diff tab.
        app.on_key(&key(KeyCode::Char('l')));
        let attempt = app.task.as_ref().unwrap().attempt.unwrap_or(0);
        let diff = read::task_diff(&self.store, target, (attempt > 0).then_some(attempt)).unwrap();
        app.apply_diff(target, attempt, Ok(diff));
        (app, target.clone(), attempt)
    }
}

#[test]
fn a_merged_task_shows_the_merge_commits_first_parent_diff() {
    let fixture = GitFixture::new();
    let (mut app, task, _) = fixture.open_diff("merged");
    let text = screen(&mut app, 120, 30);
    assert!(text.contains(&task), "{text}");
    assert!(
        text.contains("task.txt"),
        "the task's own file is listed:\n{text}"
    );
    assert!(text.contains("+1 -0"), "the counts are on the row:\n{text}");
    assert!(
        !text.contains("main.txt"),
        "main's own commit is not the task's change:\n{text}"
    );
    assert!(
        text.contains("╭─ task.txt"),
        "the selected file's diff frame is titled with its path:\n{text}"
    );
    assert!(
        text.contains("the task change"),
        "the diff body is drawn:\n{text}"
    );
    // Mutation: use `main..head` for a merged task — after the merge that is main's own
    // addition (or nothing), so the list no longer holds `task.txt`.
}

#[test]
fn a_fast_forward_merged_task_shows_every_commit_of_its_branch() {
    let fixture = GitFixture::new();
    let (mut app, task, _) = fixture.open_diff("fastforward");
    let text = screen(&mut app, 120, 30);
    assert!(text.contains(&task), "{text}");
    assert!(
        text.contains("first.txt"),
        "the branch's first commit is part of the task's change:\n{text}"
    );
    assert!(
        text.contains("second.txt"),
        "the branch's last commit is part of it too:\n{text}"
    );
    // Mutation: use `<candidate>^1..<candidate>` when the candidate is the head — the first
    // commit's file drops out of the list.
}

#[test]
fn an_unmerged_task_shows_base_to_head() {
    let fixture = GitFixture::new();
    let (mut app, _, _) = fixture.open_diff("unmerged");
    let text = screen(&mut app, 120, 30);
    assert!(
        text.contains("other.txt"),
        "the attempt's own change is listed:\n{text}"
    );
    assert!(text.contains("+2 -0"), "two added lines: {text}");
    assert!(
        text.contains("╭─ other.txt"),
        "the diff frame is titled with the path:\n{text}"
    );
    assert!(text.contains("another line"), "{text}");
    // `n` moves to the next file (and wraps nowhere: one file stays selected).
    app.on_key(&key(KeyCode::Char('n')));
    assert_eq!(app.task.as_ref().unwrap().file_sel, 0);
}

#[test]
fn the_remote_main_is_the_short_sha_of_the_remote_tracking_ref() {
    let fixture = GitFixture::new();
    let sha = read::remote_main_short(&fixture.store)
        .unwrap()
        .expect("origin/main exists");
    assert_eq!(sha.len(), 8, "{sha}");
    let full = git(&fixture.repo, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(sha, full[..8], "the header's 8-character sha");
}

#[test]
fn a_missing_task_journal_is_one_weak_line() {
    let fixture = Fixture::new();
    let mut app = fixture.app();
    app.task_sel = fixture.pos("e");
    app.open_task();
    let terminal = render(&mut app, 120, 30);
    let text = rows(&terminal).join("\n");
    assert!(text.contains("no session journal yet"), "{text}");
    let (row, column) = fg_of(&terminal, "no session journal yet");
    assert_eq!(
        fg(&terminal, row, column),
        radian_tui::theme::Theme::default().hint.fg,
        "the missing-journal line is weak"
    );
}

// ---- the plans page's columns and plan marks (ADR 0048 decision 11) ------------------

/// Three plans with different auto levels, merged counts and costs: the count and the cost end
/// at the same column on every row.
#[test]
fn the_plan_summary_columns_line_up_across_plans() {
    let scratch = Scratch::new("plan-columns");
    let store = store(&scratch.path("repo"));
    let run = add_plan(&store, "run", "the run plan");
    for index in 0..2 {
        add_task(
            &store,
            &run,
            &format!("run-{index}"),
            &[],
            TaskState::Done,
            "merged",
            1,
            Some(0.06),
        );
    }
    let merge = add_plan(&store, "merge", "the merge plan");
    for index in 0..13 {
        let (state, phase, cost) = if index < 4 {
            (TaskState::Done, "merged", (index == 0).then_some(12.40))
        } else {
            (TaskState::Todo, "ready", None)
        };
        add_task(
            &store,
            &merge,
            &format!("merge-{index}"),
            &[],
            state,
            phase,
            1,
            cost,
        );
    }
    let off = add_plan(&store, "off", "the off plan");
    for index in 0..3 {
        add_task(
            &store,
            &off,
            &format!("off-{index}"),
            &[],
            TaskState::Todo,
            "ready",
            0,
            None,
        );
    }
    for (id, auto) in [(&run, "run"), (&merge, "merge"), (&off, "off")] {
        store
            .change_plan(id, |plan| {
                plan.status = "confirmed".to_owned();
                plan.auto = auto.to_owned();
                Ok(())
            })
            .unwrap();
    }
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    // `all` keeps the finished plan (2/2) in the list, so all three set the columns.
    app.on_key(&key(KeyCode::Char('f')));
    let terminal = render(&mut app, 120, 40);
    let rows = rows(&terminal);
    // The plan's own list row: the frame's label also names the id, but it starts with `╭─`.
    let list_row = |id: &str| {
        rows.iter()
            .position(|row| row.contains(id) && !row.contains('╭'))
            .unwrap_or_else(|| panic!("no list row for {id}:\n{}", rows.join("\n")))
    };
    let column_of = |row: &str, needle: &str| -> usize {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in `{row}`"));
        row[..byte].chars().count()
    };
    let end_of = |row: &str, needle: &str| column_of(row, needle) + needle.chars().count() - 1;

    let mut count_ends = Vec::new();
    let mut cost_ends = Vec::new();
    for (id, count, cost) in [
        (&run, "2/2", "$0.12"),
        (&merge, "4/13", "$12.40"),
        (&off, "0/3", "—"),
    ] {
        let summary = &rows[list_row(id) + 1];
        assert!(
            summary.contains("run") || summary.contains("merge") || summary.contains("off"),
            "the row names its auto level: {summary}"
        );
        count_ends.push(end_of(summary, count));
        cost_ends.push(end_of(summary, cost));
    }
    assert_eq!(
        count_ends[0],
        count_ends[1],
        "the merged counts end at one column: {count_ends:?}\n{}",
        rows.join("\n")
    );
    assert_eq!(count_ends[1], count_ends[2], "{count_ends:?}");
    assert_eq!(
        cost_ends[0],
        cost_ends[1],
        "the costs end at one column: {cost_ends:?}\n{}",
        rows.join("\n")
    );
    assert_eq!(cost_ends[1], cost_ends[2], "{cost_ends:?}");
}

/// A cancelled plan and a draft plan draw `✕` / `◌`, carry no status prefix on the summary row,
/// and the cancelled plan's id and summary are weak.
#[test]
fn a_cancelled_and_a_draft_plan_draw_their_circle_marks_without_a_prefix() {
    let scratch = Scratch::new("plan-marks");
    let store = store(&scratch.path("repo"));
    let draft = add_plan(&store, "draft", "the draft plan");
    add_task(&store, &draft, "d", &[], TaskState::Todo, "ready", 0, None);
    let gone = add_plan(&store, "gone", "the cancelled plan");
    add_task(&store, &gone, "g", &[], TaskState::Todo, "ready", 0, None);
    store
        .change_plan(&gone, |plan| {
            plan.status = "cancelled".to_owned();
            plan.auto = "off".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&key(KeyCode::Char('f')));
    let theme = app.theme.clone();
    let terminal = render(&mut app, 120, 30);
    let rendered = rows(&terminal);
    let list_row = |id: &str| {
        rendered
            .iter()
            .position(|row| row.contains(id) && !row.contains('╭'))
            .unwrap_or_else(|| panic!("no list row for {id}:\n{}", rendered.join("\n")))
    };
    // The left list's summary rows carry no status prefix; the right frame's label keeps the
    // status word (ADR 0048 decision 11).
    for (id, prefix) in [(&draft, "draft · "), (&gone, "cancelled · ")] {
        let summary = &rendered[list_row(id) + 1];
        assert!(
            !summary.contains(prefix),
            "{id}'s summary row carries no `{prefix}`: {summary}"
        );
    }
    let (row, column) = mark_cell(&terminal, &draft, '◌');
    assert_eq!(fg(&terminal, row, column), theme.plan_draft.fg);
    let (row, column) = mark_cell(&terminal, &gone, '✕');
    assert_eq!(fg(&terminal, row, column), theme.plan_cancelled.fg);
    // The cancelled plan's id and summary take the weak role; the mark keeps its own.
    let (row, column) = fg_of(&terminal, &gone);
    assert_eq!(
        fg(&terminal, row, column),
        theme.hint.fg,
        "the cancelled plan's id is weak"
    );
    assert_eq!(
        fg(&terminal, row + 1, 4),
        theme.hint.fg,
        "the cancelled plan's summary is weak"
    );
}

/// One fixture plan per mark rule draws that mark in that role, and the rules are checked in
/// the decision's order: needs-you before running, running before paused.
#[test]
fn each_plan_mark_rule_draws_its_mark_and_role() {
    let scratch = Scratch::new("plan-rules");
    let store = store(&scratch.path("repo"));

    let plan = |slug: &str| add_plan(&store, slug, &format!("the {slug} plan"));
    let task = |plan: &str, slug: &str, state: TaskState, phase: &str| {
        add_task(&store, plan, slug, &[], state, phase, 1, None)
    };
    let set = |id: &str, status: &str, auto: &str| {
        store
            .change_plan(id, |plan| {
                plan.status = status.to_owned();
                plan.auto = auto.to_owned();
                Ok(())
            })
            .unwrap();
    };

    // Tasks are added while the plan is a draft: a cancelled plan takes no new task.
    let cancelled = plan("cancelled");
    task(&cancelled, "c", TaskState::Todo, "ready");
    set(&cancelled, "cancelled", "off");
    let draft = plan("draft");
    task(&draft, "d", TaskState::Todo, "ready");
    set(&draft, "draft", "off");
    let awaiting = plan("awaiting");
    task(&awaiting, "a", TaskState::Review, "awaiting_human");
    set(&awaiting, "confirmed", "run");
    let proposed = plan("proposed");
    task(&proposed, "p", TaskState::Todo, "proposed");
    set(&proposed, "confirmed", "run");
    // A proposed task plus a running one: needs you wins.
    let running_proposed = plan("running-proposed");
    task(&running_proposed, "rp", TaskState::Done, "merged");
    task(&running_proposed, "rr", TaskState::Doing, "running");
    task(&running_proposed, "rq", TaskState::Todo, "proposed");
    set(&running_proposed, "confirmed", "run");
    let running = plan("running");
    task(&running, "r", TaskState::Doing, "running");
    set(&running, "confirmed", "run");
    // Running beats paused: auto off does not hide a running task.
    let off_running = plan("off-running");
    task(&off_running, "or", TaskState::Doing, "running");
    set(&off_running, "confirmed", "off");
    let paused = plan("paused");
    task(&paused, "pa", TaskState::Todo, "ready");
    set(&paused, "confirmed", "off");
    let queued = plan("queued");
    task(&queued, "q", TaskState::Todo, "ready");
    set(&queued, "confirmed", "run");
    let done = plan("done");
    task(&done, "dn", TaskState::Done, "merged");
    task(&done, "dc", TaskState::Failed, "cancelled");
    set(&done, "confirmed", "run");
    // No task open and a failed one that is not cancelled: needs you.
    let failed_only = plan("failed-only");
    task(&failed_only, "fm", TaskState::Done, "merged");
    task(&failed_only, "fe", TaskState::Failed, "exhausted");
    set(&failed_only, "confirmed", "run");
    // A failed task with an open task left is not "needs you": the plan is queued.
    let failed_open = plan("failed-open");
    task(&failed_open, "om", TaskState::Done, "merged");
    task(&failed_open, "oe", TaskState::Failed, "exhausted");
    task(&failed_open, "oq", TaskState::Todo, "ready");
    set(&failed_open, "confirmed", "run");

    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    // `all` shows the cancelled and finished plans too.
    app.on_key(&key(KeyCode::Char('f')));
    let theme = app.theme.clone();
    let terminal = render(&mut app, 140, 60);
    let cases = [
        (&cancelled, '✕', theme.plan_cancelled),
        (&draft, '◌', theme.plan_draft),
        (&awaiting, '◉', theme.plan_needs_you),
        (&proposed, '◉', theme.plan_needs_you),
        (&running_proposed, '◉', theme.plan_needs_you),
        (&running, '●', theme.plan_running),
        (&off_running, '●', theme.plan_running),
        (&paused, '◍', theme.plan_paused),
        (&queued, '○', theme.plan_queued),
        (&done, '✓', theme.plan_done),
        (&failed_only, '◉', theme.plan_needs_you),
        (&failed_open, '○', theme.plan_queued),
    ];
    let text = rows(&terminal).join("\n");
    for (id, glyph, role) in cases {
        assert!(
            text.contains(id),
            "the plan {id} is listed even at 60 rows:\n{text}"
        );
        let (row, column) = mark_cell(&terminal, id, glyph);
        assert_eq!(
            fg(&terminal, row, column),
            role.fg,
            "{id} draws {glyph} in its role"
        );
    }
}

/// The flat task list gives the cost one column, so a `—` and a `$12.40` start their `after`
/// text at the same column.
#[test]
fn the_flat_task_list_aligns_the_after_text_across_cost_widths() {
    let scratch = Scratch::new("flat-columns");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "flat", "the flat plan");
    let base = add_task(
        &store,
        &plan,
        "base",
        &[],
        TaskState::Done,
        "merged",
        1,
        Some(0.01),
    );
    let cheap = add_task(
        &store,
        &plan,
        "cheap",
        &[&base],
        TaskState::Todo,
        "ready",
        0,
        None,
    );
    let pricey = add_task(
        &store,
        &plan,
        "pricey",
        &[&base],
        TaskState::Todo,
        "ready",
        0,
        Some(12.40),
    );
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            plan.auto = "run".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.flat = true;
    let terminal = render(&mut app, 160, 30);
    let rows = rows(&terminal);
    let after = format!("after {base}");
    let row_of = |id: &str| {
        rows.iter()
            .find(|row| row.contains(id) && !row.contains('╭'))
            .unwrap_or_else(|| panic!("no task row for {id}:\n{}", rows.join("\n")))
    };
    let column_of = |row: &str, needle: &str| {
        let byte = row
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in `{row}`"));
        row[..byte].chars().count()
    };
    let cheap_row = row_of(&cheap);
    let pricey_row = row_of(&pricey);
    assert_eq!(
        column_of(cheap_row, &after),
        column_of(pricey_row, &after),
        "the `after` text starts at one column:\n{cheap_row}\n{pricey_row}"
    );
    // The cost itself is right-aligned in its column, so the two costs end together too.
    let cost_end = |row: &str| {
        let needle = if row.contains("$12.40") {
            "$12.40"
        } else {
            "—"
        };
        let byte = row.find(needle).expect("the row carries its cost");
        row[..byte].chars().count() + needle.chars().count() - 1
    };
    assert_eq!(
        cost_end(cheap_row),
        cost_end(pricey_row),
        "the costs end at one column:\n{cheap_row}\n{pricey_row}"
    );
    assert!(
        cheap_row.contains('—') && pricey_row.contains("$12.40"),
        "{cheap_row}\n{pricey_row}"
    );
}

/// The graph task list starts `state.phase` at the same column at every depth, and a deep row's
/// id is cut with `…` when the connector leaves it less room.
#[test]
fn the_graph_task_list_aligns_columns_at_every_depth() {
    let scratch = Scratch::new("graph-columns");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "chain", "the chain plan");
    let root = add_task(
        &store,
        &plan,
        "root-task-with-a-long-name",
        &[],
        TaskState::Todo,
        "ready",
        0,
        None,
    );
    let one = add_task(
        &store,
        &plan,
        "one-task-with-a-long-name",
        &[&root],
        TaskState::Todo,
        "ready",
        0,
        None,
    );
    let two = add_task(
        &store,
        &plan,
        "two-task-with-a-long-name",
        &[&one],
        TaskState::Todo,
        "ready",
        0,
        None,
    );
    let three = add_task(
        &store,
        &plan,
        "three-task-with-a-long-name",
        &[&two],
        TaskState::Todo,
        "ready",
        0,
        None,
    );
    store
        .change_plan(&plan, |plan| {
            plan.status = "confirmed".to_owned();
            plan.auto = "run".to_owned();
            Ok(())
        })
        .unwrap();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    // The frame's right column is narrow here, so the id block is cut to fit.
    let terminal = render(&mut app, 80, 30);
    let rows = rows(&terminal);
    let phase_rows: Vec<&String> = rows
        .iter()
        .filter(|row| row.contains("todo.ready"))
        .collect();
    assert!(
        phase_rows.len() >= 4,
        "four task rows are drawn (root, one, two, three):\n{}",
        rows.join("\n")
    );
    assert!(three.starts_with("TK"));
    let column_of = |row: &str| {
        let byte = row.find("todo.ready").expect("the phase is drawn");
        row[..byte].chars().count()
    };
    for row in [phase_rows[0], phase_rows[1], phase_rows[3]] {
        assert_eq!(
            column_of(row),
            column_of(phase_rows[0]),
            "the phase column holds at every depth:\n{}",
            rows.join("\n")
        );
    }
    assert!(
        phase_rows[3].contains('…'),
        "the depth-3 row's id is cut:\n{}",
        rows.join("\n")
    );
}

/// A theme file that sets `plan_running` recolors that mark only, in the rendered page.
#[test]
fn a_theme_file_recolors_only_the_plan_mark_it_names() {
    let scratch = Scratch::new("plan-theme");
    let store = store(&scratch.path("repo"));
    let running = add_plan(&store, "running", "the running plan");
    add_task(
        &store,
        &running,
        "r",
        &[],
        TaskState::Doing,
        "running",
        1,
        None,
    );
    let queued = add_plan(&store, "queued", "the queued plan");
    add_task(&store, &queued, "q", &[], TaskState::Todo, "ready", 0, None);
    for id in [&running, &queued] {
        store
            .change_plan(id, |plan| {
                plan.status = "confirmed".to_owned();
                plan.auto = "run".to_owned();
                Ok(())
            })
            .unwrap();
    }
    let themes = scratch.path("themes");
    fs::create_dir_all(&themes).unwrap();
    fs::write(
        themes.join("mine.toml"),
        "name = \"mine\"\n[roles]\nplan_running = \"#123456\"\n",
    )
    .unwrap();
    let (theme, warnings) = radian_tui::theme::load(Some("mine"), false, true, Some(&themes), &[]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    let queued_role = app.theme.plan_queued;
    app.theme = theme;
    let terminal = render(&mut app, 120, 30);
    let (row, column) = mark_cell(&terminal, &running, '●');
    assert_eq!(
        fg(&terminal, row, column),
        Some(ratatui::style::Color::Rgb(0x12, 0x34, 0x56)),
        "the running mark takes the theme's value"
    );
    let (row, column) = mark_cell(&terminal, &queued, '○');
    assert_eq!(
        fg(&terminal, row, column),
        queued_role.fg,
        "the queued mark keeps the built-in default"
    );
}
