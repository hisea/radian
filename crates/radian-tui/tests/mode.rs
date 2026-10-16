//! ADR 0038 slice 3: the `/mode` command, as pure `App` state. No terminal, no runtime: the
//! key press is the real `App::on_key` path and the effects are what the loop would run.
//!
//! The rules pinned here: `/mode` with no argument reports the mode in force; `/mode lead`
//! switches it with no confirmation; a wrong argument is a usage notice; and an applied switch
//! moves the projection (which `/mode` and the footer read) while a refusal changes nothing.

use radian_host::Mode;
use radian_tui::app::{App, Effect, Input};
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

/// The app with the mode the host would have written into the projection.
fn app_with(mode: &str) -> App {
    let app = App::new("m", "s");
    app.session.projection.set_mode(Some(mode.to_owned()));
    app
}

/// `/mode` with no argument reports the mode in force.
#[test]
fn mode_with_no_argument_reports_the_current_mode() {
    for mode in ["general", "lead"] {
        let mut app = app_with(mode);
        app.input = "/mode".into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert!(effects.is_empty(), "{effects:?}");
        assert!(
            notices(&app)
                .iter()
                .any(|notice| notice == &format!("mode: {mode}")),
            "{:?}",
            notices(&app)
        );
    }
}

/// A mode switch is a plain effect: nothing about it is dangerous, so nothing asks first.
#[test]
fn mode_switches_without_confirmation() {
    for (command, mode) in [("/mode lead", Mode::Lead), ("/mode general", Mode::General)] {
        let mut app = app_with("general");
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert_eq!(effects, vec![Effect::SetMode { mode }], "{command}");
        assert!(app.top_modal().is_none(), "{command} must not open a panel");
    }
}

/// A wrong argument is a usage notice; more than one word is too. Nothing is queued.
#[test]
fn a_wrong_mode_argument_is_a_usage_notice() {
    for command in ["/mode maybe", "/mode lead general"] {
        let mut app = app_with("general");
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert!(effects.is_empty(), "{command}: {effects:?}");
        assert!(
            notices(&app)
                .iter()
                .any(|notice| notice.contains("usage: /mode") || notice.contains("is not a mode")),
            "{command}: {:?}",
            notices(&app)
        );
    }
}

/// An applied switch moves the projection (the UI's mirror of the mode) and says when it takes
/// effect.
#[test]
fn an_applied_switch_moves_the_projection_and_notices() {
    let mut app = app_with("general");
    let result = app.update(Input::ModeApplied {
        mode: Mode::Lead,
        result: Ok(()),
    });
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.session.projection.read().mode(), Some("lead"));
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("mode: lead from the next turn")),
        "{:?}",
        notices(&app)
    );
}

/// A refused switch (a task agent, or a session whose layer 1 is fixed) is a notice that
/// changes nothing.
#[test]
fn a_refused_switch_is_a_notice_and_changes_nothing() {
    let mut app = app_with("general");
    let result = app.update(Input::ModeApplied {
        mode: Mode::Lead,
        result: Err("a task agent's role comes from its brief and cannot be switched".into()),
    });
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.session.projection.read().mode(), Some("general"));
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("cannot switch mode")),
        "{:?}",
        notices(&app)
    );
}

/// `/mode` is on the command tree, and its one argument level offers the two modes.
#[test]
fn mode_completes_general_and_lead() {
    use radian_tui::command;
    let entry = command::tree()
        .iter()
        .find(|command| command.name == "mode")
        .expect("mode is on the tree");
    let command::Level::Ready(candidates) = (entry.levels[0])(&[], "") else {
        panic!("mode's first level is ready");
    };
    let words: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.insert.as_str())
        .collect();
    assert_eq!(words, vec!["general", "lead"]);
    assert!(
        command::HELP.contains("/mode [general|lead]"),
        "/help must list /mode"
    );
}

/// The mode names are the ones the journal, the command line and the role prompts share.
#[test]
fn the_mode_names_and_role_prompts_are_the_adr_ones() {
    assert_eq!(Mode::parse("general"), Ok(Mode::General));
    assert_eq!(Mode::parse("lead"), Ok(Mode::Lead));
    assert!(Mode::parse("task").is_err(), "a task agent is not a mode");
    assert_eq!(Mode::General.as_str(), "general");
    assert_eq!(Mode::General.role_name(), "default");
    assert_eq!(Mode::Lead.role_name(), "lead");
    // The slot parser (what the driver merges and a resume replays) agrees.
    assert_eq!(Mode::of(&serde_json::json!({"mode": "lead"})), Mode::Lead);
    for absent in [serde_json::json!({}), serde_json::json!({"mode": "nope"})] {
        assert_eq!(Mode::of(&absent), Mode::General);
    }
}
