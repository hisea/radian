//! The arc TUI's board page (ADR 0048 §1, §6 and §13): the four columns by who acts next, the
//! three widths, `d`'s finished list, `p`'s plan scope and the owner's keys on a card.
//!
//! The plans and tasks are real files in a scratch store (`Store::at`), and the records the
//! board reads (a verdict, its `review.json`, an attempt's `execution.json`, a `paused.json`)
//! are real files too, written the way arc writes them.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::gate::{Stage, Verdict, verdict_path};
use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config, plan_ops};
use radian_tui::arc::board;
use radian_tui::arc::{ArcApp, ArcKey, Page, actions, render};
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
        let dir = std::env::temp_dir().join(format!(
            "radian-arc-board-{label}-{}-{n}",
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

fn add_plan(store: &Store, slug: &str, title: &str) -> String {
    store.add_plan(slug, title, "a request\n").unwrap()
}

#[allow(clippy::too_many_arguments)]
fn add_task(
    store: &Store,
    plan: &str,
    slug: &str,
    state: TaskState,
    phase: &str,
    attempt: u32,
) -> String {
    let id = store
        .add_task(
            plan,
            slug,
            &format!("the {slug} task"),
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
    id
}

fn confirm(store: &Store, plan: &str, auto: &str) {
    store
        .change_plan(plan, |plan| {
            plan.status = "confirmed".to_owned();
            plan.auto = auto.to_owned();
            Ok(())
        })
        .unwrap();
}

/// A plan with a task in every column, plus a finished `done.merged` and a finished
/// `failed.exhausted` task. Two more plans set the claim order: `P0` first, `P3` last.
struct Fixture {
    scratch: Scratch,
    store: Store,
    first: String,
    board: String,
    manual: String,
    ids: Vec<(&'static str, String)>,
}

impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new("columns");
        let store = store(&scratch.path("repo"));

        // `P0`, auto run: its ready task is claimed before every other plan's.
        let first = add_plan(&store, "first", "the P0 plan");
        let first_ready = add_task(&store, &first, "first-ready", TaskState::Todo, "ready", 0);
        confirm(&store, &first, "run");
        plan_ops::set_priority(&store, &first, "P0").unwrap();

        // The main plan: a card in every column, in task order.
        let board = add_plan(&store, "board", "the board plan");
        let entries = [
            ("ready", TaskState::Todo, "ready", 0),
            ("proposed", TaskState::Todo, "proposed", 0),
            ("paused", TaskState::Todo, "paused_budget", 0),
            ("doing", TaskState::Doing, "running", 1),
            ("verifying", TaskState::Review, "verifying", 1),
            ("awaiting", TaskState::Review, "awaiting_human", 1),
            ("integrated", TaskState::Review, "integrating", 1),
            ("rework", TaskState::Review, "rework_pending", 1),
            ("done", TaskState::Done, "merged", 1),
            ("failed", TaskState::Failed, "exhausted", 2),
        ];
        let mut ids = Vec::new();
        for (slug, state, phase, attempt) in entries {
            let id = add_task(&store, &board, slug, state, phase, attempt);
            ids.push((slug, id));
        }
        ids.push(("first-ready", first_ready));
        confirm(&store, &board, "merge");
        plan_ops::set_priority(&store, &board, "P2").unwrap();

        // `auto off`: its awaiting_human task is the owner's.
        let manual = add_plan(&store, "manual", "the manual plan");
        let manual_ready = add_task(&store, &manual, "manual-ready", TaskState::Todo, "ready", 0);
        let manual_awaiting = add_task(
            &store,
            &manual,
            "manual-awaiting",
            TaskState::Review,
            "awaiting_human",
            1,
        );
        let manual_rework = add_task(
            &store,
            &manual,
            "manual-rework",
            TaskState::Review,
            "rework_pending",
            1,
        );
        ids.push(("manual-ready", manual_ready));
        ids.push(("manual-awaiting", manual_awaiting));
        ids.push(("manual-rework", manual_rework));
        confirm(&store, &manual, "off");

        Self {
            scratch,
            store,
            first,
            board,
            manual,
            ids,
        }
    }

    fn id(&self, slug: &str) -> &str {
        &self
            .ids
            .iter()
            .find(|(name, _)| *name == slug)
            .unwrap_or_else(|| panic!("no task `{slug}`"))
            .1
    }

    fn app(&self) -> ArcApp {
        ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.path("sessions")))
    }

    /// The app on the board, with the fixture's tasks.
    fn board(&self) -> ArcApp {
        let mut app = self.app();
        assert_eq!(app.on_key(&code(KeyCode::Char('b'))), ArcKey::Redraw);
        assert_eq!(app.page, Page::Board);
        app
    }
}

fn code(code: KeyCode) -> KeyEvent {
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

/// The row and column of the first cell of `needle`.
fn locate(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
    for (row, text) in rows(terminal).iter().enumerate() {
        if let Some(byte) = text.find(needle) {
            return (row as u16, text[..byte].chars().count() as u16);
        }
    }
    panic!("no row contains `{needle}`:\n{}", rows(terminal).join("\n"));
}

fn column_ids(app: &ArcApp, column: usize) -> Vec<&str> {
    app.board.columns[column]
        .iter()
        .map(|card| card.id.as_str())
        .collect()
}

/// The attempt's `review.json`, which names the failing stage and its verdict.
fn write_reason(fixture: &Fixture, task: &str, attempt: u32, verdict: &Path) {
    let dir = radian_arc::store::attempt_ops::run_dir(&fixture.store, task, attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(radian_arc::review::REVIEW_FILE),
        json!({
            "action": "verify",
            "stage": "commands",
            "reason": "command_failed",
            "verdict": verdict.to_string_lossy(),
            "text": "commands: command_failed",
        })
        .to_string(),
    )
    .unwrap();
}

/// A failed `commands` verdict for the task's `head_sha`, with the main it ran on. That is what
/// a `commands` or integration command failure leaves, and what a scheduling pass can reverify
/// once main moves.
fn write_command_failure(fixture: &Fixture, task: &str, attempt: u32) {
    let head = "c".repeat(40);
    fixture
        .store
        .change_task(task, |task| {
            task.head_sha = Some(head.clone());
            Ok(())
        })
        .unwrap();
    let label = format!("{task}-{attempt}");
    let mut verdict = Verdict::human(head, false, Some("command_failed".to_owned()));
    verdict.stage = Stage::Commands;
    verdict.main_sha = Some("a".repeat(40));
    let path = verdict_path(fixture.store.repo(), task, &label, Stage::Commands).unwrap();
    verdict.write_new(&path).unwrap();
    write_reason(fixture, task, attempt, &path);
}

/// The owner's reject note, which no automatic reverify follows.
fn write_reject_note(fixture: &Fixture, task: &str, attempt: u32) {
    let dir = radian_arc::store::attempt_ops::run_dir(&fixture.store, task, attempt);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(radian_arc::review::REJECT_FILE), "not like this\n").unwrap();
}

#[test]
fn every_column_holds_its_tasks_in_claim_order() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();

    assert_eq!(
        column_ids(&app, board::TODO),
        vec![
            fixture.id("first-ready"),
            fixture.id("ready"),
            fixture.id("manual-ready"),
        ],
        "todo: the P0 plan first, then the plan id, then the task order"
    );
    assert_eq!(column_ids(&app, board::DOING), vec![fixture.id("doing")]);
    assert_eq!(
        column_ids(&app, board::VERIFYING),
        vec![
            fixture.id("verifying"),
            fixture.id("awaiting"),
            fixture.id("integrated"),
        ],
        "an awaiting_human task of an auto merge plan is verifying"
    );
    assert_eq!(
        column_ids(&app, board::NEEDS_YOU),
        vec![
            fixture.id("proposed"),
            fixture.id("paused"),
            fixture.id("rework"),
            fixture.id("manual-awaiting"),
            fixture.id("manual-rework"),
        ],
        "proposed and paused_budget tasks, a rework_pending task nothing automatic will leave, \
         an awaiting_human task of an auto off plan, and a rework_pending task with a reject note"
    );
    assert!(
        app.board
            .done
            .iter()
            .any(|row| row.id == fixture.id("done")),
        "a merged task is finished: {:?}",
        app.board.done
    );
    assert!(
        app.board
            .done
            .iter()
            .any(|row| row.id == fixture.id("failed")),
        "a failed task is finished: {:?}",
        app.board.done
    );
    for column in 0..board::COLUMNS {
        assert!(
            !column_ids(&app, column)
                .iter()
                .any(|id| *id == fixture.id("done") || *id == fixture.id("failed")),
            "a finished task is not in a column"
        );
    }

    // The frame says the headings, their counts and each card.
    let text = screen(&mut app, 160, 40);
    for needle in [
        "TODO 3",
        "DOING 1",
        "VERIFYING 3",
        "NEEDS YOU 5",
        fixture.id("first-ready"),
        fixture.id("doing"),
        fixture.id("awaiting"),
        fixture.id("proposed"),
        &format!("{} · rework_pending", fixture.board),
        &format!("{} · awaiting_human", fixture.manual),
        "all plans",
    ] {
        assert!(text.contains(needle), "missing `{needle}`:\n{text}");
    }
    // A needs-you card carries the `!` mark.
    let terminal = render(&mut app, 160, 40);
    let (row, column) = locate(&terminal, fixture.id("paused"));
    let line = rows(&terminal)[row as usize].clone();
    let before: String = line.chars().take(column as usize).collect();
    assert!(before.contains('!'), "the needs-you mark: {line}");
}

#[test]
fn an_awaiting_human_task_is_the_owners_only_at_auto_off() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();

    // The same phase in two plans: `auto merge` merges it, `auto off` waits for the owner.
    assert!(column_ids(&app, board::VERIFYING).contains(&fixture.id("awaiting")));
    assert!(!column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("awaiting")));
    assert!(column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("manual-awaiting")));
    assert!(!column_ids(&app, board::VERIFYING).contains(&fixture.id("manual-awaiting")));

    // The plan's level decides it, not the card: change it and the task moves.
    fixture
        .store
        .change_plan(&fixture.board, |plan| {
            plan.auto = "run".to_owned();
            Ok(())
        })
        .unwrap();
    app.refresh();
    assert!(
        column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("awaiting")),
        "auto run stops at awaiting_human, so the owner merges it"
    );
}

#[test]
fn a_rework_pending_task_with_an_automatic_reverify_is_not_in_needs_you() {
    let fixture = Fixture::new();
    write_command_failure(&fixture, fixture.id("rework"), 1);
    // The reject note's task has the same command failure, in a plan whose auto level is not
    // `off`, so only the note can keep it out of `verifying`.
    fixture
        .store
        .change_plan(&fixture.manual, |plan| {
            plan.auto = "run".to_owned();
            Ok(())
        })
        .unwrap();
    write_command_failure(&fixture, fixture.id("manual-rework"), 1);
    let mut app = fixture.board();
    app.rebuild_board();
    assert!(
        column_ids(&app, board::VERIFYING).contains(&fixture.id("rework")),
        "a scheduling pass reverifies it by itself once main moves: {:?}",
        app.board.columns
    );
    assert!(
        column_ids(&app, board::VERIFYING).contains(&fixture.id("manual-rework")),
        "without a note the command failure is one a pass can reverify: {:?}",
        app.board.columns
    );

    write_reject_note(&fixture, fixture.id("manual-rework"), 1);
    app.refresh();
    assert!(
        column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("manual-rework")),
        "a reject note is no automatic rework: {:?}",
        app.board.columns
    );

    // With the task's automatic reverifies used up, nothing automatic is coming.
    let dir = fixture.store.root().join("runs").join(fixture.id("rework"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("auto_reverify.json"),
        json!({
            "reverified": [
                {"head_sha": "c".repeat(40), "main_sha": "d".repeat(40), "at": "2026-10-20T10:00:00Z"},
                {"head_sha": "c".repeat(40), "main_sha": "e".repeat(40), "at": "2026-10-20T11:00:00Z"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    app.refresh();
    assert!(
        column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("rework")),
        "the automatic reverify count is used up: {:?}",
        app.board.columns
    );
}

/// A `(head_sha, main)` pair the pass re-verified already cannot be revisited until main moves,
/// so the card waits for the owner instead of claiming a gate is coming.
#[test]
fn a_command_failure_whose_pair_was_reverified_waits_for_the_owner() {
    let fixture = Fixture::new();
    write_command_failure(&fixture, fixture.id("rework"), 1);
    let mut app = fixture.board();
    app.rebuild_board();
    assert!(
        column_ids(&app, board::VERIFYING).contains(&fixture.id("rework")),
        "the first look at the failure has a pass coming: {:?}",
        app.board.columns
    );

    // The pass already re-verified this head against the main the failing verdict recorded.
    let dir = fixture.store.root().join("runs").join(fixture.id("rework"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("auto_reverify.json"),
        json!({
            "reverified": [
                {"head_sha": "c".repeat(40), "main_sha": "a".repeat(40), "at": "2026-10-20T10:00:00Z"}
            ]
        })
        .to_string(),
    )
    .unwrap();
    app.refresh();
    assert!(
        column_ids(&app, board::NEEDS_YOU).contains(&fixture.id("rework")),
        "the pair is spent: {:?}",
        app.board.columns
    );
}

#[test]
fn d_lists_merged_and_failed_tasks_newest_first() {
    let fixture = Fixture::new();
    let merged = fixture.id("done").to_owned();
    let failed = fixture.id("failed").to_owned();
    // A merged task's finished time is its passing integration verdict.
    let label = format!("{merged}-1");
    let mut integration = Verdict::human("c".repeat(40), true, None);
    integration.stage = Stage::Integration;
    integration.candidate_sha = Some("d".repeat(40));
    integration.base_sha = Some("a".repeat(40));
    integration.ended_at = "2026-10-20T10:00:00.000Z".to_owned();
    integration
        .write_new(
            &verdict_path(fixture.store.repo(), &merged, &label, Stage::Integration).unwrap(),
        )
        .unwrap();
    // A failed task's is its attempt's own end record.
    let dir = radian_arc::store::attempt_ops::run_dir(&fixture.store, &failed, 2);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("execution.json"),
        json!({ "ended_at": "2026-10-22T10:00:00.000Z" }).to_string(),
    )
    .unwrap();

    let mut app = fixture.board();
    app.rebuild_board();
    assert_eq!(app.on_key(&code(KeyCode::Char('d'))), ArcKey::Redraw);
    assert!(app.board.finished);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("FINISHED"), "{text}");
    let newest = text.find(&failed).expect("the failed row");
    let oldest = text.find(&merged).expect("the merged row");
    assert!(
        newest < oldest,
        "the most recently finished comes first:\n{text}"
    );

    // `d` again (and `Esc`) returns to the columns.
    assert_eq!(app.on_key(&code(KeyCode::Char('d'))), ArcKey::Redraw);
    assert!(!app.board.finished);
    assert!(screen(&mut app, 120, 30).contains("TODO"));
    app.on_key(&code(KeyCode::Char('d')));
    assert_eq!(app.on_key(&code(KeyCode::Esc)), ArcKey::Redraw);
    assert!(!app.board.finished);
    assert!(app.board.selected_done().is_some());
}

#[test]
fn a_medium_width_shows_two_columns_and_a_narrow_one_stacks_the_four_groups() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();

    // Medium: 80 cells hold two 28-cell columns but not four.
    let terminal = render(&mut app, 80, 30);
    assert_eq!(app.board.layout, board::BoardLayout::Medium);
    let (todo_row, todo_column) = locate(&terminal, "TODO");
    let (doing_row, doing_column) = locate(&terminal, "DOING");
    let (verifying_row, verifying_column) = locate(&terminal, "VERIFYING");
    let (needs_row, needs_column) = locate(&terminal, "NEEDS YOU");
    assert!(
        todo_column < 40 && doing_column < 40,
        "todo over doing on the left: {todo_column}, {doing_column}"
    );
    assert!(
        verifying_column >= 40 && needs_column >= 40,
        "verifying over needs you on the right: {verifying_column}, {needs_column}"
    );
    assert!(
        todo_row < doing_row && verifying_row < needs_row,
        "each pane stacks its groups: {todo_row} < {doing_row}, {verifying_row} < {needs_row}"
    );
    // `l` moves to the other pane, `j` stays inside this one.
    assert_eq!(app.on_key(&code(KeyCode::Char('l'))), ArcKey::Redraw);
    assert_eq!(app.board.column, board::VERIFYING);
    assert_eq!(app.on_key(&code(KeyCode::Char('h'))), ArcKey::Redraw);
    assert_eq!(app.board.column, board::TODO);
    assert_eq!(app.on_key(&code(KeyCode::Char('j'))), ArcKey::Redraw);
    assert_eq!(
        app.board.column,
        board::TODO,
        "j walks the pane's groups, so the first move is still todo"
    );

    // Narrow: 40 cells hold one column.
    let terminal = render(&mut app, 40, 40);
    assert_eq!(app.board.layout, board::BoardLayout::Narrow);
    let (todo_row, _) = locate(&terminal, "TODO");
    let (doing_row, _) = locate(&terminal, "DOING");
    let (verifying_row, _) = locate(&terminal, "VERIFYING");
    let (needs_row, _) = locate(&terminal, "NEEDS YOU");
    assert!(
        todo_row < doing_row && doing_row < verifying_row && verifying_row < needs_row,
        "the four groups stack in order: {todo_row} {doing_row} {verifying_row} {needs_row}"
    );
    // `l` jumps to the next group.
    let before = app.board.column;
    assert_eq!(app.on_key(&code(KeyCode::Char('l'))), ArcKey::Redraw);
    assert_eq!(app.board.column, (before + 1).min(3));
}

#[test]
fn p_limits_the_board_to_the_selected_cards_plan() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();
    // The P0 plan's ready task is the first `todo` card.
    assert_eq!(app.board.column, board::TODO);
    assert_eq!(
        app.board.selected().map(|card| card.id.as_str()),
        Some(fixture.id("first-ready"))
    );

    assert_eq!(app.on_key(&code(KeyCode::Char('p'))), ArcKey::Redraw);
    assert_eq!(app.board.plan.as_deref(), Some(fixture.first.as_str()));
    let text = screen(&mut app, 160, 40);
    assert!(text.contains(&fixture.first), "{text}");
    assert!(!text.contains("all plans"), "{text}");
    assert!(!text.contains(fixture.id("manual-ready")), "{text}");
    assert!(!text.contains(fixture.id("ready")), "{text}");
    assert!(
        app.board.selected().map(|card| card.id.as_str()) == Some(fixture.id("first-ready")),
        "the selection stays on the card it pinned"
    );

    assert_eq!(app.on_key(&code(KeyCode::Char('p'))), ArcKey::Redraw);
    assert_eq!(app.board.plan, None);
    let text = screen(&mut app, 160, 40);
    assert!(text.contains("all plans"), "{text}");
    assert!(text.contains(fixture.id("manual-ready")), "{text}");
}

#[test]
fn enter_opens_the_selected_tasks_page_and_esc_returns_to_the_board() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.board.column = board::VERIFYING;
    app.board.row = 0;
    let expected = fixture.id("verifying").to_owned();
    assert_eq!(app.on_key(&code(KeyCode::Enter)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Task);
    assert_eq!(
        app.task.as_ref().map(|page| page.id.clone()),
        Some(expected)
    );
    assert_eq!(app.on_key(&code(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Board);
    assert!(screen(&mut app, 120, 30).contains("TODO"));
}

#[test]
fn an_owner_key_on_a_card_behaves_as_on_the_plans_page() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();

    // `x` asks first, with the task form of the cancel.
    app.board.column = board::NEEDS_YOU;
    app.board.row = 0;
    let proposed = fixture.id("proposed").to_owned();
    assert_eq!(app.on_key(&code(KeyCode::Char('x'))), ArcKey::Redraw);
    match app.float.clone() {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert_eq!(message, format!("cancel {proposed} ?"));
            assert_eq!(action, actions::Action::Cancel(proposed.clone()));
        }
        other => panic!("no cancel float: {other:?}"),
    }
    assert_eq!(app.on_key(&code(KeyCode::Char('n'))), ArcKey::Redraw);
    assert!(app.float.is_none());

    // `c` confirms a proposed task.
    assert_eq!(app.on_key(&code(KeyCode::Char('c'))), ArcKey::Redraw);
    assert_eq!(
        app.take_action(),
        Some(actions::Action::Confirm(proposed.clone()))
    );
    app.pending = None;

    // `s` starts a ready task.
    app.board.column = board::TODO;
    app.board.row = 0;
    let ready = fixture.id("first-ready").to_owned();
    assert_eq!(app.on_key(&code(KeyCode::Char('s'))), ArcKey::Redraw);
    assert_eq!(app.take_action(), Some(actions::Action::Start(ready)));
    app.pending = None;

    // `s` on a task that cannot start shows the plans page's line and queues nothing.
    app.board.column = board::VERIFYING;
    app.board.row = 0;
    assert_eq!(app.on_key(&code(KeyCode::Char('s'))), ArcKey::Redraw);
    assert_eq!(app.take_action(), None);
    assert!(
        app.result
            .as_ref()
            .is_some_and(|line| line.text.contains("only a todo.ready task starts")),
        "{:?}",
        app.result
    );

    // `a` cycles the card's plan; setting `merge` asks first, as on the plans page.
    app.board.column = board::TODO;
    app.board.row = 0; // the P0 plan at auto `run`
    assert_eq!(app.on_key(&code(KeyCode::Char('a'))), ArcKey::Redraw);
    match app.float.clone() {
        Some(radian_tui::arc::Float::Confirm { message, action }) => {
            assert_eq!(message, format!("set auto merge on {}?", fixture.first));
            assert_eq!(
                action,
                actions::Action::Auto {
                    plan: fixture.first.clone(),
                    level: "merge".to_owned(),
                }
            );
        }
        other => panic!("no auto float: {other:?}"),
    }
    assert_eq!(app.on_key(&code(KeyCode::Char('n'))), ArcKey::Redraw);

    // `M` opens the merge page of a card in `review.*`.
    app.board.column = board::VERIFYING;
    app.board.row = 0;
    assert_eq!(app.on_key(&code(KeyCode::Char('M'))), ArcKey::Redraw);
    assert_eq!(app.page, Page::Merge);
    assert_eq!(
        app.merge.as_ref().map(|page| page.id.as_str()),
        Some(fixture.id("verifying"))
    );
    assert_eq!(app.on_key(&code(KeyCode::Esc)), ArcKey::Redraw);
    assert_eq!(app.page, Page::Board, "the merge page returns to the board");
}

#[test]
fn the_selection_stays_on_the_same_task_across_a_refresh() {
    let fixture = Fixture::new();
    let mut app = fixture.board();
    app.rebuild_board();
    let selected = fixture.id("first-ready").to_owned();
    assert_eq!(
        app.board.selected().map(|card| card.id.clone()),
        Some(selected.clone())
    );

    // Another plan's task moves: the selection stays.
    fixture
        .store
        .change_task(fixture.id("manual-ready"), |task| {
            task.state = TaskState::Doing;
            task.phase = "running".to_owned();
            Ok(())
        })
        .unwrap();
    app.refresh();
    assert_eq!(
        app.board.selected().map(|card| card.id.clone()),
        Some(selected)
    );

    // The selected task leaves the board: the selection moves to the nearest card there.
    fixture
        .store
        .change_task(fixture.id("first-ready"), |task| {
            task.state = TaskState::Done;
            task.phase = "merged".to_owned();
            Ok(())
        })
        .unwrap();
    app.refresh();
    assert_eq!(app.board.column, board::TODO);
    assert_eq!(app.board.row, 0);
    assert_eq!(
        app.board.selected().map(|card| card.id.as_str()),
        Some(fixture.id("ready")),
        "the nearest todo card"
    );
}

#[test]
fn a_long_column_scrolls_to_keep_the_selected_card_visible() {
    let scratch = Scratch::new("scroll");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "many", "many tasks");
    let ids: Vec<String> = (1..=10)
        .map(|n| {
            add_task(
                &store,
                &plan,
                &format!("many-{n:02}"),
                TaskState::Todo,
                "ready",
                0,
            )
        })
        .collect();
    confirm(&store, &plan, "off");
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&code(KeyCode::Char('b')));
    // Twelve rows leave room for the heading and two cards.
    let text = screen(&mut app, 40, 12);
    assert!(text.contains(&ids[0]), "{text}");
    assert!(!text.contains(&ids[9]), "{text}");
    for _ in 0..9 {
        assert_eq!(app.on_key(&code(KeyCode::Char('j'))), ArcKey::Redraw);
    }
    let text = screen(&mut app, 40, 12);
    assert!(
        text.contains(&ids[9]),
        "the selected card is drawn:\n{text}"
    );
    assert!(!text.contains(&ids[0]), "the column scrolled:\n{text}");
}

/// A plan budget pauses the whole plan (`auto` becomes `off` and the stopped task stays in
/// `todo.paused_budget`), so every open task waits for the owner. A task budget leaves the
/// plan's level alone: only the stopped task waits.
#[test]
fn a_plan_budget_pause_puts_every_task_in_needs_you_but_a_task_budget_does_not() {
    let scratch = Scratch::new("budget-pause");
    let store = store(&scratch.path("repo"));

    let paused = add_plan(&store, "paused", "the paused plan");
    let paused_budget = add_task(
        &store,
        &paused,
        "paused-budget",
        TaskState::Todo,
        "paused_budget",
        1,
    );
    let paused_ready = add_task(&store, &paused, "paused-ready", TaskState::Todo, "ready", 0);
    let paused_doing = add_task(
        &store,
        &paused,
        "paused-doing",
        TaskState::Doing,
        "running",
        1,
    );
    confirm(&store, &paused, "off");

    let running = add_plan(&store, "running", "the running plan");
    let task_budget = add_task(
        &store,
        &running,
        "task-budget",
        TaskState::Todo,
        "paused_budget",
        1,
    );
    let running_ready = add_task(
        &store,
        &running,
        "running-ready",
        TaskState::Todo,
        "ready",
        0,
    );
    let running_doing = add_task(
        &store,
        &running,
        "running-doing",
        TaskState::Doing,
        "running",
        1,
    );
    confirm(&store, &running, "run");

    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&code(KeyCode::Char('b')));
    app.rebuild_board();
    let needs = column_ids(&app, board::NEEDS_YOU);
    for id in [&paused_budget, &paused_ready, &paused_doing, &task_budget] {
        assert!(
            needs.contains(&id.as_str()),
            "{id} needs the owner: {:?}",
            app.board.columns
        );
    }
    assert!(
        column_ids(&app, board::TODO).contains(&running_ready.as_str()),
        "a task budget keeps the plan's other tasks running: {:?}",
        app.board.columns
    );
    assert!(
        column_ids(&app, board::DOING).contains(&running_doing.as_str()),
        "a task budget keeps the plan's other tasks running: {:?}",
        app.board.columns
    );
}

/// The finished times come from records with different fractional widths: a verdict writes
/// microseconds, `execution.json` milliseconds. The list orders by the instant, not the text.
#[test]
fn d_orders_records_of_one_second_by_their_instant_not_their_fraction_width() {
    let scratch = Scratch::new("finished-order");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "times", "the times plan");
    let merged = add_task(&store, &plan, "merged", TaskState::Done, "merged", 1);
    let failed = add_task(&store, &plan, "failed", TaskState::Failed, "exhausted", 1);
    confirm(&store, &plan, "off");

    // 100.001 ms is after 100 ms, although the millisecond string sorts after the microsecond
    // one; the merged task takes its time from the integration verdict, the failed one from
    // the wrapper's `execution.json`.
    let label = format!("{merged}-1");
    let mut integration = Verdict::human("c".repeat(40), true, None);
    integration.stage = Stage::Integration;
    integration.candidate_sha = Some("d".repeat(40));
    integration.base_sha = Some("a".repeat(40));
    integration.ended_at = "2026-10-20T10:00:00.100001Z".to_owned();
    integration
        .write_new(&verdict_path(store.repo(), &merged, &label, Stage::Integration).unwrap())
        .unwrap();
    let dir = radian_arc::store::attempt_ops::run_dir(&store, &failed, 1);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("execution.json"),
        json!({ "ended_at": "2026-10-20T10:00:00.100Z" }).to_string(),
    )
    .unwrap();

    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&code(KeyCode::Char('b')));
    app.rebuild_board();
    assert_eq!(
        app.board
            .done
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>(),
        vec![merged, failed],
        "the later instant comes first: {:?}",
        app.board.done
    );
}

/// The placeholder line is not a finished task: an empty list counts `0`.
#[test]
fn an_empty_finished_list_counts_no_row() {
    let scratch = Scratch::new("finished-empty");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "open", "an open plan");
    add_task(&store, &plan, "ready", TaskState::Todo, "ready", 0);
    confirm(&store, &plan, "off");
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&code(KeyCode::Char('b')));
    assert_eq!(app.on_key(&code(KeyCode::Char('d'))), ArcKey::Redraw);
    let text = screen(&mut app, 120, 30);
    assert!(text.contains("FINISHED 0"), "{text}");
    assert!(text.contains("nothing finished"), "{text}");
}

#[test]
fn an_empty_column_says_so() {
    let scratch = Scratch::new("empty");
    let store = store(&scratch.path("repo"));
    let plan = add_plan(&store, "one", "one task");
    add_task(&store, &plan, "ready", TaskState::Todo, "ready", 0);
    confirm(&store, &plan, "off");
    let mut app = ArcApp::with_session_dir(store, Some(scratch.path("sessions")));
    app.on_key(&code(KeyCode::Char('b')));
    let text = screen(&mut app, 160, 30);
    assert!(text.contains("nothing running"), "{text}");
    assert!(text.contains("nothing verifying"), "{text}");
    assert!(text.contains("nothing needs you"), "{text}");
    assert!(text.contains("TODO 1"), "{text}");
}
