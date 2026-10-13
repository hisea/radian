//! The slash-command layer in `App::on_key`: `/help`, `/memory`, the `//` escape, confirmations
//! and unknown commands (ADR 0021 D7). No process is started.

use std::path::PathBuf;

use radian_host::memory::{MemoryService, ScopeSelection, WriteScope};
use radian_memory::Kind;
use radian_tui::app::{App, EntryKind, Send, TurnState};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-tui-command-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An `App` wired to a fresh project memory service.
fn app_with_memory(label: &str) -> (PathBuf, App, MemoryService) {
    let dir = scratch(label);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let service = MemoryService::open(&ws, dir.join("memory"));
    service.create_project().unwrap();
    let mut app = App::new("m", "s");
    app.set_memory(Some(service.clone()));
    (dir, app, service)
}

fn notices(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn submit(app: &mut App, line: &str) -> Option<Send> {
    app.input = line.into();
    app.on_key(key(KeyCode::Enter))
}

#[test]
fn help_and_unbalanced_quotes_are_notices_and_send_nothing() {
    let mut app = App::new("m", "s");
    assert_eq!(submit(&mut app, "/help"), None);
    assert!(
        notices(&app).iter().any(|n| n.contains("/memory")),
        "{:?}",
        notices(&app)
    );

    assert_eq!(submit(&mut app, "/unknown"), None);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("unknown command /unknown")),
        "{:?}",
        notices(&app)
    );

    assert_eq!(submit(&mut app, "/memory add \"unbalanced"), None);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("unbalanced quotes")),
        "{:?}",
        notices(&app)
    );
}

#[test]
fn a_double_slash_sends_the_message_with_one_slash() {
    let mut app = App::new("m", "s");
    assert_eq!(submit(&mut app, "//hi"), Some(Send::Followup("/hi".into())));
    app.turn = TurnState::Running;
    assert_eq!(
        submit(&mut app, "//steer me"),
        Some(Send::Steer("/steer me".into()))
    );
}

#[test]
fn a_memory_read_runs_inline_and_is_never_sent() {
    let (dir, mut app, service) = app_with_memory("list");
    service
        .add(WriteScope::Project, Kind::Fact, None, "hello world".into())
        .unwrap();
    assert_eq!(submit(&mut app, "/memory list"), None);
    assert!(
        notices(&app).iter().any(|n| n.contains("hello world")),
        "{:?}",
        notices(&app)
    );

    // While a turn runs a read command still runs inline and does not steer the turn.
    app.turn = TurnState::Running;
    assert_eq!(submit(&mut app, "/memory list"), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_confirmed_forget_waits_for_y_and_n_keeps_the_record() {
    let (dir, mut app, service) = app_with_memory("forget");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "keep me".into())
        .unwrap();

    assert_eq!(
        submit(&mut app, &format!("/memory forget {}", record.id)),
        None
    );
    assert!(app.pending_command.is_some(), "the command waits for y/n");
    assert!(
        notices(&app).iter().any(|n| n.contains("[y/N]")),
        "{:?}",
        notices(&app)
    );

    // `n` cancels; the record is untouched.
    app.on_key(key(KeyCode::Char('n')));
    assert!(app.pending_command.is_none());
    assert_eq!(
        service
            .list(ScopeSelection::ProjectOnly, false)
            .unwrap()
            .len(),
        1
    );

    // `y` runs it.
    assert_eq!(
        submit(&mut app, &format!("/memory forget {}", record.id)),
        None
    );
    app.on_key(key(KeyCode::Char('y')));
    assert!(app.pending_command.is_none());
    assert!(
        notices(&app).iter().any(|n| n.contains("forgot")),
        "{:?}",
        notices(&app)
    );
    assert!(
        service
            .list(ScopeSelection::ProjectOnly, false)
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pending_confirmation_keeps_keys_out_of_the_input_box() {
    let (dir, mut app, service) = app_with_memory("keys");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "keep".into())
        .unwrap();
    submit(&mut app, &format!("/memory forget {}", record.id));
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.input.is_empty(),
        "a pending confirmation swallows typing"
    );
    assert!(app.pending_command.is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.pending_command.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}
