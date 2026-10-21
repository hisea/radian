//! `/btw`: the side question's pure `App` behavior (backlog
//! `tui-btw-side-question`). No terminal, no runtime, no files: submit lines,
//! press keys, feed the loop's inputs, and assert the effects, the overlay and
//! the notices.

use radian_host::btw::SideOutcome;
use radian_tui::app::{App, Effect, EntryKind, Input};
use radian_tui::btw::BtwState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Submits one line and returns the effects the key produced (the loop would run them).
fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    app.on_key(key(KeyCode::Enter)).effects
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn overlay_text(app: &App) -> Option<String> {
    app.btw().map(|overlay| overlay.text.clone())
}

/// `/btw <question>` opens the streaming overlay and asks for the side request.
/// Nothing reaches the transcript: the line is a command, not a message.
#[test]
fn btw_with_a_question_opens_the_overlay_and_asks() {
    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/btw what file was that");
    assert_eq!(
        effects,
        vec![Effect::SideQuestion {
            question: "what file was that".into()
        }]
    );
    let overlay = app.btw().expect("the overlay opens");
    assert_eq!(overlay.question, "what file was that");
    assert_eq!(overlay.state, BtwState::Streaming);
    assert!(
        notices(&app).is_empty(),
        "no notice for a well-formed /btw: {:?}",
        notices(&app)
    );
}

/// A second `/btw` while one runs is refused with a notice: at most one side
/// request at a time.
#[test]
fn a_second_btw_while_one_runs_is_refused() {
    let mut app = App::new("m", "s");
    let first = submit(&mut app, "/btw first question");
    assert_eq!(first.len(), 1, "the first /btw runs");
    let second = submit(&mut app, "/btw second question");
    assert!(second.is_empty(), "no second effect while one runs");
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("already running")),
        "{:?}",
        notices(&app)
    );
    // The first overlay is untouched by the refusal.
    assert_eq!(app.btw().unwrap().question, "first question");
}

/// Esc while the side request runs cancels it and closes the overlay.
#[test]
fn esc_while_running_cancels_and_closes() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    let result = app.on_key(key(KeyCode::Esc));
    assert_eq!(result.effects, vec![Effect::CancelSideQuestion]);
    assert!(app.btw().is_none(), "the overlay closes on cancel");
    // The late outcome for the cancelled request is ignored.
    let result = app.update(Input::SideDone(SideOutcome::Cancelled {
        partial: "half".into(),
    }));
    assert!(result.effects.is_empty());
    assert!(app.btw().is_none(), "no overlay reopens for a cancel");
}

/// Esc on a finished overlay just closes it.
#[test]
fn esc_on_a_finished_overlay_closes_it() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    app.update(Input::SideDone(SideOutcome::Done {
        text: "the file".into(),
        tool_calls_dropped: false,
        usage: None,
        fee_micros: None,
    }));
    let result = app.on_key(key(KeyCode::Esc));
    assert!(result.effects.is_empty(), "closing sends no effect");
    assert!(app.btw().is_none());
}

/// Streamed deltas accumulate in the overlay; the finished answer stores the
/// last answer and shows the usage line.
#[test]
fn deltas_stream_and_done_finishes_with_the_usage_line() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    app.update(Input::SideDelta("the ".into()));
    app.update(Input::SideDelta("file".into()));
    assert_eq!(overlay_text(&app).as_deref(), Some("the file"));
    app.update(Input::SideDone(SideOutcome::Done {
        text: "the file".into(),
        tool_calls_dropped: true,
        usage: None,
        fee_micros: None,
    }));
    let overlay = app.btw().expect("the overlay stays open when done");
    assert_eq!(overlay.state, BtwState::Done);
    // `/btw` alone reopens exactly this answer.
    app.on_key(key(KeyCode::Esc));
    assert!(app.btw().is_none());
    let effects = submit(&mut app, "/btw");
    assert!(effects.is_empty(), "reopening needs no effect");
    let overlay = app.btw().expect("the last answer reopens");
    assert_eq!(overlay.text, "the file");
    assert_eq!(overlay.state, BtwState::Done);
}

/// A delta for a request that is no longer running is dropped.
#[test]
fn a_late_delta_is_dropped() {
    let mut app = App::new("m", "s");
    app.update(Input::SideDelta("late".into()));
    assert!(app.btw().is_none(), "no overlay opens for a late delta");
}

/// `/btw` alone with no previous answer is a notice, not an overlay.
#[test]
fn btw_alone_with_no_previous_answer_is_a_notice() {
    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/btw");
    assert!(effects.is_empty());
    assert!(app.btw().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("no previous side answer")),
        "{:?}",
        notices(&app)
    );
}

/// A failed side request shows the error in the overlay only: the session keeps
/// running, the transcript gains no failure notice, and no turn fails (there is
/// no ADR 0017 path for a side request).
#[test]
fn a_failed_side_request_shows_only_in_the_overlay() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    let before: Vec<String> = app
        .session
        .entries
        .iter()
        .map(|entry| entry.text.clone())
        .collect();
    app.update(Input::SideDone(SideOutcome::Failed("boom".into())));
    let overlay = app.btw().expect("the overlay shows the failure");
    assert!(matches!(overlay.state, BtwState::Failed(_)));
    let after: Vec<String> = app
        .session
        .entries
        .iter()
        .map(|entry| entry.text.clone())
        .collect();
    assert_eq!(before, after, "the transcript is untouched by the failure");
}

/// A side question is allowed while a turn runs: the overlay opens and the
/// effect goes out, and the main turn is not cancelled.
#[test]
fn btw_is_allowed_mid_turn() {
    let mut app = App::new("m", "s");
    app.session
        .projection
        .set_turn(radian_tui::TurnState::Running);
    let effects = submit(&mut app, "/btw what file was that");
    assert_eq!(
        effects,
        vec![Effect::SideQuestion {
            question: "what file was that".into()
        }]
    );
    assert!(app.btw().is_some(), "the overlay opens mid-turn");
}

/// PgDn scrolls the streamed answer; the overlay renders the asked question and
/// the text so far.
#[test]
fn the_overlay_scrolls_and_renders() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    for _ in 0..50 {
        app.update(Input::SideDelta(
            "a long line of streamed answer text\n".into(),
        ));
    }
    app.on_key(key(KeyCode::PageDown));
    assert!(
        app.btw().unwrap().scroll > 0,
        "PgDn moves the answer window"
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, &mut app))
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol().to_owned())
        .collect();
    assert!(screen.contains("what file was that"), "the question shows");
    assert!(screen.contains("streamed answer"), "the answer shows");
}

/// A finished overlay renders the usage line and the tools note.
#[test]
fn the_finished_overlay_renders_usage_and_the_tools_note() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/btw what file was that");
    app.update(Input::SideDone(SideOutcome::Done {
        text: "the file".into(),
        tool_calls_dropped: true,
        usage: Some(radian_core::ModelUsage {
            input: Some(10),
            output: Some(3),
            ..Default::default()
        }),
        fee_micros: None,
    }));
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, &mut app))
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol().to_owned())
        .collect();
    assert!(screen.contains("10 in"), "the usage line shows: {screen}");
    assert!(
        screen.contains(radian_host::btw::NO_TOOL_LINE),
        "the tools note shows: {screen}"
    );
}
