//! ADR 0038 slice 2: the `/access` command, as pure `App` state. No terminal, no runtime:
//! the key press is the real `App::on_key` path and the effects are what the loop would run.
//!
//! The rules pinned here: `/access` with no argument reports the tier and its source; a switch
//! to a sandboxed tier is direct; a switch to `full` (no sandbox) needs confirmation and the
//! default focus is `cancel`; and an applied switch moves the footer's tier and the `!`/`!!`
//! availability.

use radian_tui::app::{AccessChoice, App, Effect, Input, Modal};
use radian_tui::profile::{AccessSource, Tier};
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

fn submit(command: &str) -> App {
    let mut app = App::new("m", "s");
    app.set_tier("workspace");
    app.input = command.into();
    let _ = app.on_key(key(KeyCode::Enter));
    app
}

/// `/access` with no argument reports the current tier and where it came from.
#[test]
fn access_with_no_argument_reports_the_tier_and_source() {
    let mut app = App::new("m", "s");
    app.set_tier("full (project)");
    app.set_access_source(AccessSource::Project);
    app.input = "/access".into();
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("full (project)") && notice.contains("from project")),
        "{:?}",
        notices(&app)
    );
}

/// A switch to a sandboxed tier needs no confirmation: it is an effect the loop applies.
#[test]
fn access_to_a_sandboxed_tier_switches_without_confirmation() {
    for command in ["/access safe", "/access workspace"] {
        let app = submit(command);
        assert!(app.top_modal().is_none(), "{command} must not open a panel");
    }
    // The effect itself, checked directly (the helper above consumed it).
    let mut app = App::new("m", "s");
    app.set_tier("full");
    app.input = "/access safe".into();
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::SetAccess {
            tier: Tier::Safe,
            remember: false,
        }],
        "{effects:?}"
    );
}

/// Switching to `full` (no sandbox) opens the confirmation, and the default focus is `cancel`:
/// Enter alone does not switch.
#[test]
fn access_full_asks_first_and_the_default_is_cancel() {
    let mut app = submit("/access full");
    assert!(
        matches!(app.top_modal(), Some(Modal::AccessFull(_))),
        "the confirmation must be up"
    );
    // Enter with the default (cancel) leaves the tier unchanged: no effect.
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert!(effects.is_empty(), "{effects:?}");
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("access unchanged")),
        "{:?}",
        notices(&app)
    );
}

/// Esc cancels the confirmation and leaves the tier unchanged.
#[test]
fn esc_cancels_the_full_confirmation() {
    let mut app = submit("/access full");
    assert!(matches!(app.top_modal(), Some(Modal::AccessFull(_))));
    let effects = app.on_key(key(KeyCode::Esc)).effects;
    assert!(effects.is_empty(), "{effects:?}");
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("access unchanged")),
        "{:?}",
        notices(&app)
    );
}

/// Choosing "this session only" and "remember for this project" switch to `full`; the
/// `remember` flag differs.
#[test]
fn access_full_choices_switch_with_the_right_remember_flag() {
    // Choices are [session, remember, cancel] with the default on cancel (index 2).
    let mut app = submit("/access full");
    app.on_key(key(KeyCode::Up)); // remember
    app.on_key(key(KeyCode::Up)); // session
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::SetAccess {
            tier: Tier::FullAccess,
            remember: false,
        }],
        "{effects:?}"
    );

    let mut app = submit("/access full");
    app.on_key(key(KeyCode::Up)); // remember
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::SetAccess {
            tier: Tier::FullAccess,
            remember: true,
        }],
        "{effects:?}"
    );
}

/// An applied switch moves the footer's tier (with the session/project marker) and the `!`/`!!`
/// availability (`safe` has no shell).
#[test]
fn an_applied_switch_updates_the_footer_and_the_shell_flag() {
    let mut app = App::new("m", "s");
    app.set_tier("workspace");
    app.set_bash_available(true);

    let result = app.update(Input::AccessApplied {
        tier: Tier::Safe,
        remember: false,
        result: Ok(()),
    });
    assert!(result.effects.is_empty());
    assert_eq!(app.session.projection.read().tier(), Some("safe (session)"));
    assert!(!app.session.bash_available, "safe has no shell");
    // ADR 0036: the tool list (and the shell description) is fixed when
    // the session opens, so the switch says so instead of leaving the text stale.
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("fixed for this session")),
        "{:?}",
        notices(&app)
    );

    let result = app.update(Input::AccessApplied {
        tier: Tier::FullAccess,
        remember: true,
        result: Ok(()),
    });
    assert!(result.effects.is_empty());
    assert_eq!(app.session.projection.read().tier(), Some("full (project)"));
    assert!(app.session.bash_available, "full has a shell");
}

/// A refused switch (a failed sandbox probe) is a notice, and the footer is untouched.
#[test]
fn a_refused_switch_is_a_notice_and_changes_nothing() {
    let mut app = App::new("m", "s");
    app.set_tier("workspace");
    app.set_bash_available(true);
    let result = app.update(Input::AccessApplied {
        tier: Tier::FullAccess,
        remember: false,
        result: Err("the sandbox backend is unavailable".into()),
    });
    assert!(result.effects.is_empty());
    assert_eq!(app.session.projection.read().tier(), Some("workspace"));
    assert!(app.session.bash_available);
    assert!(
        notices(&app)
            .iter()
            .any(|notice| notice.contains("cannot switch access")),
        "{:?}",
        notices(&app)
    );
}

/// A wrong argument is a usage notice; more than one name is too.
#[test]
fn a_wrong_access_argument_is_a_usage_notice() {
    for command in ["/access maybe", "/access safe extra"] {
        let mut app = App::new("m", "s");
        app.set_tier("workspace");
        app.input = command.into();
        let effects = app.on_key(key(KeyCode::Enter)).effects;
        assert!(effects.is_empty(), "{command}: {effects:?}");
        assert!(
            notices(&app).iter().any(|notice| {
                notice.contains("usage: /access") || notice.contains("not one of safe")
            }),
            "{command}: {:?}",
            notices(&app)
        );
    }
}

/// `/access` is on the command tree, and its one argument level offers the three tiers.
#[test]
fn access_completes_the_three_tiers() {
    use radian_tui::command;
    let entry = command::tree()
        .iter()
        .find(|command| command.name == "access")
        .expect("access is on the tree");
    let command::Level::Ready(candidates) = (entry.levels[0])(&[], "") else {
        panic!("access's first level is ready");
    };
    let words: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.insert.as_str())
        .collect();
    assert_eq!(words, vec!["safe", "workspace", "full"]);
}

/// The confirmation's choices are exactly the three the ADR names, and the default is `cancel`.
#[test]
fn the_confirmation_choices_and_default_are_the_adr_ones() {
    use radian_tui::app::AccessWizard;
    assert_eq!(
        AccessChoice::ALL,
        [
            AccessChoice::Session,
            AccessChoice::Remember,
            AccessChoice::Cancel
        ]
    );
    assert_eq!(AccessWizard::new().selected(), AccessChoice::Cancel);
}
