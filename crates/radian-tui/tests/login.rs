//! The `/login` and `/logout` flows in `App::update` (ADR 0026 D3): the wizard is pure state,
//! the network and the `auth.json` write are effects, and the confirmation gates the removal.
//! No process is started, no file is written here (the loop does that).

use std::collections::VecDeque;

use radian_tui::app::{App, Effect, Input, Modal};
use radian_tui::login::{LoginKey, LoginStep, LoginWizard};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    let mut seen = Vec::new();
    let mut queue: VecDeque<Effect> = app.on_key(key(KeyCode::Enter)).effects.into();
    while let Some(effect) = queue.pop_front() {
        seen.push(effect.clone());
    }
    seen
}

fn notices(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::app::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn top_login(app: &App) -> &LoginWizard {
    match app.top_modal() {
        Some(Modal::Login(wizard)) => wizard,
        None => panic!("no modal on top: expected the login wizard"),
        Some(_) => panic!("expected the login wizard on top, got a different modal"),
    }
}

/// `/login` opens the provider choice; `/login <builtin>` jumps to its key; an unknown name
/// is a notice.
#[test]
fn login_opens_the_wizard_and_a_builtin_skips_to_the_key() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login").is_empty());
    assert!(matches!(
        top_login(&app).step,
        LoginStep::ChooseProvider { .. }
    ));

    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login anthropic").is_empty());
    assert!(matches!(top_login(&app).step, LoginStep::KeyInput));
    assert_eq!(top_login(&app).provider_name(), "anthropic");

    // `custom` starts at the name; an unknown id is refused with a notice, no wizard.
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login custom").is_empty());
    assert!(matches!(top_login(&app).step, LoginStep::CustomName));

    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login corp").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("unknown provider")),
        "{:?}",
        notices(&app)
    );
    assert!(app.top_modal().is_none());
}

/// The whole custom walk through the wizard's own input: name → protocol → URL → key, ending
/// in a verify effect (D3), and a successful verification turns into the save effect (D10's
/// env on the record).
#[test]
fn the_custom_walk_ends_in_verify_then_save() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login custom").is_empty());

    // The name: a reserved one is an inline error and stays; a valid one moves on.
    for c in "openai-compatible".chars() {
        app.update(Input::LoginKey(LoginKey::Char(c)));
    }
    app.update(Input::LoginKey(LoginKey::Enter));
    assert!(
        top_login(&app).error.is_some(),
        "a reserved name is refused"
    );
    for _ in 0.."openai-compatible".len() {
        app.update(Input::LoginKey(LoginKey::Backspace));
    }
    for c in "corp".chars() {
        app.update(Input::LoginKey(LoginKey::Char(c)));
    }
    app.update(Input::LoginKey(LoginKey::Enter));
    assert!(matches!(
        top_login(&app).step,
        LoginStep::CustomProtocol { .. }
    ));

    // The protocol: Enter takes the highlighted row (openai-compatible).
    app.update(Input::LoginKey(LoginKey::Enter));
    assert!(matches!(top_login(&app).step, LoginStep::CustomBaseUrl));

    // The URL: an invalid one errors inline, a good one moves to the key.
    for c in "corp.example.com".chars() {
        app.update(Input::LoginKey(LoginKey::Char(c)));
    }
    app.update(Input::LoginKey(LoginKey::Enter));
    assert!(top_login(&app).error.is_some());
    for _ in 0.."corp.example.com".len() {
        app.update(Input::LoginKey(LoginKey::Backspace));
    }
    for c in "http://127.0.0.1:9/v1".chars() {
        app.update(Input::LoginKey(LoginKey::Char(c)));
    }
    app.update(Input::LoginKey(LoginKey::Enter));
    assert!(matches!(top_login(&app).step, LoginStep::KeyInput));

    // The key — via paste, like a password manager would (D3: paste is masked too).
    let effects = app
        .update(Input::LoginKey(LoginKey::Paste("sk-pty-1".to_owned())))
        .effects;
    assert!(effects.is_empty(), "paste does not submit");
    let effects = submit_login_key(&mut app);
    assert_eq!(
        effects,
        vec![Effect::LoginVerify {
            name: "corp".to_owned(),
            protocol: "openai-compatible",
            base_url: "http://127.0.0.1:9/v1".to_owned(),
            key: "sk-pty-1".to_owned(),
        }]
    );
    assert!(matches!(top_login(&app).step, LoginStep::Verifying));

    // The 200 → the save, with the endpoint and protocol in the record's env (D10).
    let effects = app
        .update(Input::LoginVerified {
            name: "corp".to_owned(),
            result: Ok(()),
        })
        .effects;
    assert_eq!(
        effects,
        vec![Effect::LoginSave {
            name: "corp".to_owned(),
            key: "sk-pty-1".to_owned(),
            env: vec![
                (
                    radian_host::RADIAN_BASE_URL.to_owned(),
                    "http://127.0.0.1:9/v1".to_owned()
                ),
                (
                    radian_host::RADIAN_KIND.to_owned(),
                    "openai-compatible".to_owned()
                ),
            ],
        }]
    );

    // The save result closes the wizard and says what to do next (D3).
    assert!(
        app.update(Input::LoginSaved {
            name: "corp".to_owned(),
            result: Ok(()),
        })
        .effects
        .is_empty()
    );
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("saved the key for `corp`") && n.contains("/model corp/")),
        "{:?}",
        notices(&app)
    );
}

/// Submits the key field (Enter through the modal's hardcoded path).
fn submit_login_key(app: &mut App) -> Vec<Effect> {
    app.on_key(key(KeyCode::Enter)).effects
}

/// A failed verification asks; `y` saves anyway (D3), and the wizard is gone after the save.
#[test]
fn a_failed_verification_can_still_be_saved() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login openai").is_empty());
    for c in "sk-oai".chars() {
        app.update(Input::LoginKey(LoginKey::Char(c)));
    }
    let effects = submit_login_key(&mut app);
    assert!(matches!(effects.first(), Some(Effect::LoginVerify { .. })));

    // The failure: the wizard stays up and asks; no key left the app.
    assert!(
        app.update(Input::LoginVerified {
            name: "openai".to_owned(),
            result: Err("https://api.openai.com/v1/models rejected the key (HTTP 401)".to_owned()),
        })
        .effects
        .is_empty()
    );
    assert!(matches!(
        top_login(&app).step,
        LoginStep::VerifyFailed { .. }
    ));

    // `y` (hardcoded) saves anyway.
    let effects = app.on_key(key(KeyCode::Char('y'))).effects;
    assert!(matches!(effects.first(), Some(Effect::LoginSave { .. })));
}

/// Esc cancels from any step and nothing is saved; a late verification result is dropped.
#[test]
fn esc_cancels_and_a_late_answer_is_dropped() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/login custom").is_empty());
    app.update(Input::LoginKey(LoginKey::Esc));
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("nothing was saved")),
        "{:?}",
        notices(&app)
    );

    // No wizard waiting: a late `LoginVerified` is a no-op.
    let mut app = App::new("m", "s");
    assert!(
        app.update(Input::LoginVerified {
            name: "x".to_owned(),
            result: Ok(()),
        })
        .effects
        .is_empty()
    );
    assert!(app.top_modal().is_none());
}

/// `/logout <provider>` confirms; `y` removes (the loop's effect), `n` keeps.
#[test]
fn logout_confirms_then_removes_or_keeps() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/logout").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("usage: /logout")),
        "{:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/logout corp");
    assert!(effects.is_empty(), "the confirmation is not an effect yet");
    assert!(app.pending_logout().is_some());

    // `n` keeps it; nothing was sent to the host.
    app.on_key(key(KeyCode::Char('n')));
    assert!(app.pending_logout().is_none());
    assert!(
        notices(&app).iter().any(|n| n.contains("kept `corp`")),
        "{:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/logout corp").is_empty());
    let effects = app.on_key(key(KeyCode::Char('y'))).effects;
    assert_eq!(
        effects,
        vec![Effect::Logout {
            provider: "corp".to_owned(),
        }]
    );
    assert!(app.pending_logout().is_none());
    // The removal result names the provider either way.
    app.update(Input::LogoutDone {
        provider: "corp".to_owned(),
        result: Ok(()),
    });
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("removed the stored credential for `corp`")),
        "{:?}",
        notices(&app)
    );
}

/// The completion fetch for `/logout` is a host fetch (the stored credentials live in
/// `auth.json`, which `update` must not read).
#[test]
fn logout_completion_needs_the_host() {
    use radian_tui::command::{self, Level};
    let tree = command::tree();
    let Some(level) = tree
        .iter()
        .find(|command| command.name == "logout")
        .map(|command| command.levels[0])
    else {
        panic!("logout is in the tree");
    };
    match level(&[], "") {
        Level::Needs(fetch) => assert!(matches!(fetch, command::Fetch::LoggedInProviders { .. })),
        other => panic!("expected Needs, got {other:?}"),
    }
}
