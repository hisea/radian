//! The arc TUI's flow column, task-page field, waiting reason and per-flow running counts
//! (ADR 0048 decisions 11, 13 and 15): a repository with two flows shows them, and a
//! repository without `[flows]` renders exactly as it did before flows existed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use radian_arc::store::model::TaskState;
use radian_arc::store::{Store, config};
use radian_tui::arc::{ArcApp, Page, render};
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
            "radian-arc-flows-{label}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("the temp dir is creatable");
        Self { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A store in `dir` with `config` as its `config.toml`.
fn store_with(dir: &Path, config_text: &str) -> Store {
    let root = dir.join(".radian/arc");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("config.toml"), config_text).unwrap();
    Store::at(dir)
}

/// `config::default_text` with two extra flows: `code` runs `radian exec` with the default
/// profile, and `image` gates with `review` + `human`. Both hold one running task at a time.
fn two_flow_config() -> String {
    let mut text = config::default_text("main");
    text = text.replace("concurrency = 1", "concurrency = 2");
    text.push_str(
        r#"
[gate.profiles.review]
stages = [{ stage = "review", required = true }, { stage = "human", required = true }]

[flows.code]
concurrency = 1

[flows.image]
concurrency = 1
gate_profile = "review"
"#,
    );
    text
}

fn add_plan(store: &Store, slug: &str, title: &str) -> String {
    store.add_plan(slug, title, "a request\n").unwrap()
}

#[allow(clippy::too_many_arguments)]
fn add_task(
    store: &Store,
    plan: &str,
    slug: &str,
    flow: Option<&str>,
    state: TaskState,
    phase: &str,
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
            task.flow = flow.map(str::to_owned);
            task.state = state;
            task.phase = phase.to_owned();
            Ok(())
        })
        .unwrap();
    id
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn screen(app: &mut ArcApp, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, app))
        .expect("the frame draws");
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
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rendered row of `id`.
fn row<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(id))
        .unwrap_or_else(|| panic!("no row for `{id}`:\n{text}"))
}

/// A confirmed plan at `auto run` with two flowing tasks and one plain one.
struct Flows {
    scratch: Scratch,
    store: Store,
    plan: String,
    first_ready: String,
    first_running: String,
    second_ready: String,
    second_running: String,
    plain: String,
}

impl Flows {
    fn new(config_text: String, with_flows: bool) -> Self {
        let scratch = Scratch::new(if with_flows { "two-flows" } else { "no-flows" });
        let store = store_with(&scratch.dir.join("repo"), &config_text);
        let plan = add_plan(&store, "route", "the routing plan");
        let (first_ready, first_running, second_ready, second_running, plain) = if with_flows {
            (
                add_task(
                    &store,
                    &plan,
                    "first-ready",
                    Some("code"),
                    TaskState::Todo,
                    "ready",
                ),
                add_task(
                    &store,
                    &plan,
                    "first-running",
                    Some("code"),
                    TaskState::Doing,
                    "running",
                ),
                add_task(
                    &store,
                    &plan,
                    "second-ready",
                    Some("image"),
                    TaskState::Todo,
                    "ready",
                ),
                add_task(
                    &store,
                    &plan,
                    "second-running",
                    Some("image"),
                    TaskState::Doing,
                    "running",
                ),
                add_task(&store, &plan, "plain", None, TaskState::Todo, "ready"),
            )
        } else {
            (
                add_task(&store, &plan, "first-ready", None, TaskState::Todo, "ready"),
                add_task(
                    &store,
                    &plan,
                    "first-running",
                    None,
                    TaskState::Doing,
                    "running",
                ),
                add_task(
                    &store,
                    &plan,
                    "second-ready",
                    None,
                    TaskState::Todo,
                    "ready",
                ),
                add_task(
                    &store,
                    &plan,
                    "second-running",
                    None,
                    TaskState::Doing,
                    "running",
                ),
                add_task(&store, &plan, "plain", None, TaskState::Todo, "ready"),
            )
        };
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
            first_ready,
            first_running,
            second_ready,
            second_running,
            plain,
        }
    }

    fn app(&self) -> ArcApp {
        ArcApp::with_session_dir(self.store.clone(), Some(self.scratch.dir.join("sessions")))
    }
}

/// A repository with two flows: the plan rows carry the flow column and the waiting reason,
/// the task page names the flow, and the board's `DOING` heading counts running per flow.
#[test]
fn two_flows_show_the_column_the_field_the_reason_and_the_counts() {
    let flows = Flows::new(two_flow_config(), true);
    let mut app = flows.app();
    assert_eq!(
        app.selected_plan().map(|view| &view.plan.item.id),
        Some(&flows.plan)
    );
    let text = screen(&mut app, 160, 40);

    // Every row has the flow column, in one width (`default` is the widest name).
    for (id, flow, phase) in [
        (&flows.first_ready, "code", "todo.ready"),
        (&flows.first_running, "code", "doing.running"),
        (&flows.second_ready, "image", "todo.ready"),
        (&flows.second_running, "image", "doing.running"),
        (&flows.plain, "default", "todo.ready"),
    ] {
        let line = row(&text, id);
        assert!(
            line.contains(&format!("{flow:<7} {phase}")),
            "the flow column before the phase: {line}"
        );
    }

    // The waiting reason is `show`'s own text, named by the limit that is full.
    assert!(text.contains("(waiting for a free code slot)"), "{text}");
    assert!(text.contains("(waiting for a free image slot)"), "{text}");
    assert!(
        text.contains("(waiting for a free concurrency slot)"),
        "a task whose flow has room waits on the repository total:\n{text}"
    );

    // The task page names the flow always, `default` included.
    app.task_sel = app
        .task_rows(app.selected_plan().unwrap())
        .iter()
        .position(|row| row.id == flows.first_ready)
        .unwrap();
    app.open_task();
    let text = screen(&mut app, 160, 30);
    assert!(text.contains("flow code"), "{text}");
    app.close_task();
    app.task_sel = app
        .task_rows(app.selected_plan().unwrap())
        .iter()
        .position(|row| row.id == flows.plain)
        .unwrap();
    app.open_task();
    let text = screen(&mut app, 160, 30);
    assert!(text.contains("flow default"), "{text}");
    app.close_task();

    // The board's DOING heading adds each flow with running tasks after the total.
    app.on_key(&key(KeyCode::Char('b')));
    assert_eq!(app.page, Page::Board);
    let text = screen(&mut app, 160, 40);
    assert!(
        text.contains("DOING 2 · code 1/1 · image 1/1"),
        "the overview runs per flow:\n{text}"
    );
}

/// A repository without `[flows]` renders as it did before flows existed: no flow column and
/// no per-flow counts. The task page still names the flow, which is always `default`.
#[test]
fn a_repository_without_flows_renders_as_before() {
    let flows = Flows::new(config::default_text("main"), false);
    let mut app = flows.app();
    assert!(
        app.flows.is_none(),
        "a repository with only the default flow has no flow view"
    );
    let text = screen(&mut app, 160, 40);

    // The row goes from the id straight to the phase: no flow word between them.
    for id in [
        &flows.first_ready,
        &flows.first_running,
        &flows.second_ready,
        &flows.second_running,
        &flows.plain,
    ] {
        let line = row(&text, id);
        assert!(!line.contains("default"), "{line}");
        assert!(!line.contains("code"), "no flow column: {line}");
        assert!(!line.contains("image"), "no flow column: {line}");
    }
    assert!(
        !text.contains("default"),
        "the whole page draws no flow word:\n{text}"
    );

    // The board keeps the plain total, with no per-flow counts.
    app.on_key(&key(KeyCode::Char('b')));
    let text = screen(&mut app, 160, 40);
    assert!(text.contains("DOING 2"), "{text}");
    assert!(
        !text.contains("DOING 2 ·"),
        "no per-flow counts without a second flow:\n{text}"
    );

    // The task page names `default`, as it always does.
    app.on_key(&key(KeyCode::Char('b')));
    app.task_sel = app
        .task_rows(app.selected_plan().unwrap())
        .iter()
        .position(|row| row.id == flows.plain)
        .unwrap();
    app.open_task();
    let text = screen(&mut app, 160, 30);
    assert!(text.contains("flow default"), "{text}");
}
