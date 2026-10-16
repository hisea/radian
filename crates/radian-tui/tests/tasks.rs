//! ADR 0030 slice 6b2: the TUI's view of task agents — the `TaskSpawned`/`TaskFinished` notice
//! lines, the status-bar count (absent with no tasks), and `/tasks`.
//!
//! Pure `App` state on a `TestBackend`: the event is fed exactly as the loop feeds it, and the
//! key press is the real `App::on_key` path.

use radian_session::RuntimeEvent;
use radian_session::TurnOutcome;
use radian_tui::app::{App, Effect, Input};
use radian_tui::command::{Outcome, TasksRequest};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn screen(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

fn spawned(id: &str) -> RuntimeEvent {
    RuntimeEvent::TaskSpawned {
        agent_session_id: id.to_owned(),
        worktree: Some(format!("/work/{id}")),
        label: None,
    }
}

fn finished(id: &str) -> RuntimeEvent {
    RuntimeEvent::TaskFinished {
        agent_session_id: id.to_owned(),
        outcome: TurnOutcome::Completed,
        error: None,
    }
}

/// A `TaskSpawned` becomes a notice line and makes the status-bar count appear; a
/// `TaskFinished` becomes a notice and makes it disappear.
#[test]
fn task_events_become_notices_and_move_the_count() {
    let mut app = App::new("mock", "s");
    assert!(app.session.task_counts().is_none(), "no tasks at startup");
    let before = screen(&mut app, 140, 12);
    assert!(
        !before.contains("tasks"),
        "no count with no tasks: {before}"
    );

    let dirty = app.update(Input::Runtime(spawned("t1")));
    assert!(dirty.dirty, "a spawn must redraw");
    assert_eq!(app.session.task_counts(), Some((1, 1)));
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("task `t1` spawned")),
        "{:?}",
        notices(&app)
    );
    let with_task = screen(&mut app, 140, 12);
    assert!(with_task.contains("tasks 1/1"), "{with_task}");

    let dirty = app.update(Input::Runtime(finished("t1")));
    assert!(dirty.dirty, "a finish must redraw");
    assert!(app.session.task_counts().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("task `t1` finished")),
        "{:?}",
        notices(&app)
    );
    let after = screen(&mut app, 140, 12);
    assert!(!after.contains("tasks"), "the count is gone: {after}");
}

/// `/tasks` with no argument lists; `cancel`/`kill` carry the task id; anything else is a usage
/// notice. Parsing only, so nothing is run here.
#[test]
fn tasks_parses_list_cancel_and_kill() {
    assert!(matches!(
        radian_tui::command::parse("/tasks", true),
        Outcome::Tasks(TasksRequest::List)
    ));
    assert!(matches!(
        radian_tui::command::parse("/tasks cancel t1", true),
        Outcome::Tasks(TasksRequest::Cancel(id)) if id == "t1"
    ));
    assert!(matches!(
        radian_tui::command::parse("/tasks kill t1", true),
        Outcome::Tasks(TasksRequest::Kill(id)) if id == "t1"
    ));
    match radian_tui::command::parse("/tasks nope", true) {
        Outcome::Notices(notices) => {
            assert!(notices[0].contains("usage: /tasks"), "{notices:?}")
        }
        other => panic!("expected a usage notice, got {other:?}"),
    }
}

/// A submitted `/tasks …` line is the host effect the loop runs.
#[test]
fn a_submitted_tasks_line_is_the_loop_effect() {
    let mut app = App::new("mock", "s");
    app.input = "/tasks cancel t1".into();
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::Tasks(TasksRequest::Cancel("t1".to_owned()))],
        "{effects:?}"
    );
}

/// `/tasks` is on the command tree; its first level offers `cancel` and `kill`, and `/help`
/// lists it.
#[test]
fn tasks_completes_cancel_and_kill() {
    let entry = radian_tui::command::tree()
        .iter()
        .find(|command| command.name == "tasks")
        .expect("tasks is on the tree");
    let radian_tui::command::Level::Ready(candidates) = (entry.levels[0])(&[], "") else {
        panic!("tasks' first level is ready");
    };
    let words: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.insert.as_str())
        .collect();
    assert_eq!(words, vec!["cancel", "kill"]);
    assert!(
        radian_tui::command::HELP.contains("/tasks"),
        "/help must list /tasks"
    );
}
