//! The pure `App::update` (ADR 0023 U3): every effect's trigger, and what each result `Input`
//! does to the state. No terminal, no runtime, no files.

use agent_core::CompactionOutcome;
use radian_tui::app::{App, Effect, Input, MemoryResult, TurnState};
use radian_tui::command::{ExportRequest, SessionOutcome};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

/// Submits one line and returns the effects the key produced (the loop would run them).
fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    app.on_key(key(KeyCode::Enter)).effects
}

fn notices(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// Every `/…` command that needs the outside world asks for it with one effect, and never
/// sends anything itself.
#[test]
fn every_slash_command_produces_its_effect() {
    let mut app = App::new("m", "s");
    app.set_memory_enabled(true);

    assert_eq!(
        submit(&mut app, "/compact"),
        vec![Effect::Compact { instructions: None }]
    );
    assert_eq!(
        submit(&mut app, "/compact focus on the parser"),
        vec![Effect::Compact {
            instructions: Some("focus on the parser".into())
        }]
    );
    assert_eq!(
        submit(&mut app, "/sessions"),
        vec![Effect::Session(SessionOutcome::List(None))]
    );
    assert_eq!(
        submit(&mut app, "/sessions parser bug"),
        vec![Effect::Session(SessionOutcome::List(Some(
            "parser bug".into()
        )))]
    );
    assert_eq!(
        submit(&mut app, "/export json out.json --force"),
        vec![Effect::Session(SessionOutcome::Export(ExportRequest {
            format: radian_host::Format::Json,
            path: Some("out.json".into()),
            force: true,
        }))]
    );
    assert_eq!(
        submit(&mut app, "/fork"),
        vec![Effect::Session(SessionOutcome::Fork)]
    );
    let memory = submit(&mut app, "/memory list");
    assert!(
        matches!(
            memory.as_slice(),
            [Effect::RunMemoryCommand {
                confirmed: false,
                ..
            }]
        ),
        "{memory:?}"
    );
}

/// ADR 0022 §1: `/fork` needs an idle session. The refusal is a decision on app state, so it
/// is made in `update` (a notice, no effect).
#[test]
fn fork_is_refused_while_a_turn_runs() {
    let mut app = App::new("m", "s");
    app.turn = TurnState::Running;
    let effects = submit(&mut app, "/fork");
    assert!(effects.is_empty(), "no fork while running: {effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("finish the current turn before forking")),
        "{:?}",
        notices(&app)
    );
}

/// The result inputs: each one changes the state the way the loop's result used to.
#[test]
fn the_result_inputs_change_the_state() {
    let mut app = App::new("m", "s");

    // A manual `/compact` outcome: compacted, nothing to do, and a failure.
    app.update(Input::CompactDone(Ok(CompactionOutcome::Compacted {
        revision: 1,
        tokens_before: 100,
        tokens_after: 40,
    })));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction: 100 → 40 tokens"),
        "{:?}",
        notices(&app)
    );
    app.update(Input::CompactDone(Ok(CompactionOutcome::NothingToCompact)));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction: nothing to compact")
    );
    app.update(Input::CompactDone(Err("boom".into())));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction failed: boom")
    );

    // `/sessions`, `/export` and `/fork` results are lines.
    app.update(Input::SessionDone(vec!["one".into(), "two".into()]));
    assert!(
        notices(&app).iter().any(|note| note == "one")
            && notices(&app).iter().any(|note| note == "two")
    );

    // A `/memory` output, and a failure (including "memory is disabled").
    app.update(Input::MemoryCommandDone(MemoryResult::Output(vec![
        "out".into(),
    ])));
    assert!(notices(&app).iter().any(|note| note == "out"));
    app.update(Input::MemoryCommandDone(MemoryResult::Failed(
        "memory is disabled (no RADIAN_HOME or HOME)".into(),
    )));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("memory is disabled"))
    );
}

/// A runtime turn failure is a transcript notice; a recognizable context overflow adds the
/// one-line help (ADR 0018's fallback).
#[test]
fn a_failed_turn_is_a_notice_and_an_overflow_adds_the_help() {
    let mut app = App::new("m", "s");
    app.update(Input::Runtime(radian_session::RuntimeEvent::TurnFailed {
        turn_id: "t-1".into(),
        message: "boom".into(),
    }));
    assert!(
        notices(&app).iter().any(|note| note == "turn failed: boom"),
        "{:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    app.update(Input::Runtime(radian_session::RuntimeEvent::TurnFailed {
        turn_id: "t-1".into(),
        message: format!("{} HTTP 400", radian_host::CONTEXT_OVERFLOW_PREFIX),
    }));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == radian_host::CONTEXT_OVERFLOW_HELP),
        "{:?}",
        notices(&app)
    );
}

/// Paste, resize and the `/memory` confirmation answer.
#[test]
fn paste_resize_and_the_confirmation_answer() {
    let mut app = App::new("m", "s");
    app.update(Input::Paste("one\ntwo".into()));
    assert_eq!(app.input, "one\ntwo");
    assert!(
        app.update(Input::Resize {
            rows: 40,
            cols: 100
        })
        .dirty
    );

    // `y`/`n` without a pending command do nothing at all.
    assert!(app.update(Input::ConfirmCommand(true)).effects.is_empty());
    assert!(app.update(Input::ConfirmCommand(false)).effects.is_empty());
}

/// `dirty` is false when nothing the renderer reads changed, so the loop can skip the frame
/// (ADR 0023 U2's zero-redraw rule rests on this).
#[test]
fn an_input_that_changes_nothing_is_not_dirty() {
    let mut app = App::new("m", "s");
    // An unbound key is not an input at all.
    let none = app.on_key(key(KeyCode::F(5)));
    assert!(!none.dirty && none.effects.is_empty());
    // Cancel and quit leave the frame alone (the turn state changes when the turn ends).
    app.turn = TurnState::Running;
    let cancel = app.on_key(ctrl(KeyCode::Char('c')));
    assert_eq!(cancel.effects, vec![Effect::Cancel]);
    assert!(!cancel.dirty, "cancelling does not repaint");
    app.turn = TurnState::Idle;
    assert!(
        !app.on_key(ctrl(KeyCode::Char('c'))).dirty,
        "quitting neither"
    );
    // Typing does.
    assert!(app.on_key(key(KeyCode::Char('a'))).dirty);
}
