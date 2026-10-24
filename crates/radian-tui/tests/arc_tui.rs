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
use radian_tui::arc::{ArcApp, ArcKey, Focus};
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
        "auto off · 1/1 · $0.12".to_owned(),
        "PL0002-images".to_owned(),
        "draft · auto off · 0/2 · —".to_owned(),
        format!(
            "{} confirmed · auto merge · 3/8 merged · $0.61",
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
fn the_plan_list_scrolls_to_keep_the_selected_plan_visible() {
    let scratch = Scratch::new("plan-scroll");
    let store = store(&scratch.path("repo"));
    let ids: Vec<String> = (1..=12)
        .map(|n| add_plan(&store, &format!("plan-{n:02}"), &format!("plan {n}")))
        .collect();
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    // Four plans fit under the heading at this height; the rest are below the window.
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
        rows(&terminal)[4].contains(&ids[8]),
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
    let theme = app.theme.clone();
    let terminal = render(&mut app, 120, 30);
    // A cancelled plan is not a confirmed one: it takes the failure mark, not `●`.
    let (row, column) = mark_cell(&terminal, "PL0002-images", '×');
    assert_eq!(fg(&terminal, row, column), theme.status_error.fg);
    let text = rows(&terminal).join("\n");
    assert!(
        text.contains("cancelled · auto off · 0/2 · —"),
        "the list names the status:\n{text}"
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
    let mut app = fixture.app();
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
    // The plan list's own marks: ✓ all merged (green), ● confirmed with work left (accent),
    // ◻ draft (weak).
    check("PL0001-first", '✓', theme.status_ok, "a finished plan");
    check(&fixture.plan, '●', theme.model, "a confirmed plan");
    check("PL0002-images", '◻', theme.hint, "a draft plan");
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
    assert!(!row.contains('❯'), "no cursor without focus: {row}");

    app.focus = Focus::Tasks;
    let text = screen(&mut app, 120, 30);
    let row = text
        .lines()
        .find(|line| line.contains(fixture.id("c")))
        .expect("the c row is drawn");
    assert!(
        row.contains("❯ "),
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
        "conversation",
        "diff",
        "attempts",
        "attempt 1  2",
        "no session journal yet",
    ] {
        assert!(text.contains(needle), "missing `{needle}`:\n{text}");
    }
    // Tab goes back to the plans page; Tab again reopens the task page.
    assert_eq!(app.on_key(&key(KeyCode::Tab)), ArcKey::Redraw);
    assert_eq!(app.page, radian_tui::arc::Page::Plans);
    assert!(screen(&mut app, 120, 30).contains("PLANS"));
    app.on_key(&key(KeyCode::Tab));
    assert!(screen(&mut app, 120, 30).contains("conversation"));
    // `q` leaves from either page.
    assert_eq!(app.on_key(&key(KeyCode::Char('q'))), ArcKey::Quit);
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
