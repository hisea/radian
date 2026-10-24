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
    app.session
        .entries
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

/// A frame on `TestBackend` (the wizard has no rendering test otherwise).
fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
        .expect("terminal");
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .expect("draw");
    terminal.backend().to_string()
}

/// The shared panel shape (ADR 0035) for the wizard: the provider choice through
/// the shared list, at 80 and 120 columns and in the ascii tier.
#[test]
fn the_login_panel_snapshot_at_80_120_and_ascii() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/login");

    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("╭─ /login "), "{screen}");
    assert!(screen.contains("choose a provider"), "{screen}");
    assert!(
        screen.contains("❯ opencode-go"),
        "the cursor is on the first choice: {screen}"
    );
    assert!(
        screen.contains("↑/↓ choose · ⏎ next · esc cancel"),
        "{screen}"
    );

    // The description column only exists above 40 columns (ADR 0035).
    let screen = draw(&mut app, 120, 24);
    assert!(screen.contains("╭─ /login "), "{screen}");
    assert!(screen.contains("OpenCode Go"), "{screen}");

    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".into()),
        ..Default::default()
    });
    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("+- /login "), "{screen}");
    assert!(screen.contains("> opencode-go"), "{screen}");
    assert!(!screen.contains('╭'), "{screen}");
}

/// The shared panel shape (ADR 0035) for the `/logout` confirmation.
#[test]
fn the_logout_panel_snapshot_at_80_120_and_ascii() {
    let mut app = App::new("m", "s");
    submit(&mut app, "/logout corp");
    app.update(Input::LogoutChoices {
        provider: "corp".to_owned(),
        choices: Ok(vec![radian_host::KeyChoice {
            name: "first".to_owned(),
            masked: "sk-…irst".to_owned(),
        }]),
    });

    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("╭─ /logout "), "{screen}");
    assert!(
        screen.contains("remove the stored credential for `corp`?"),
        "{screen}"
    );
    assert!(screen.contains("╰─ [y] remove · [n/esc] keep"), "{screen}");

    let screen = draw(&mut app, 120, 24);
    assert!(screen.contains("╭─ /logout "), "{screen}");

    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".into()),
        ..Default::default()
    });
    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("+- /logout "), "{screen}");
    assert!(
        screen.contains("| remove the stored credential"),
        "{screen}"
    );
    assert!(!screen.contains('╭'), "{screen}");
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
            existing: false,
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
            add: None,
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
            existing: false,
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
            existing: false,
        })
        .effects
        .is_empty()
    );
    assert!(app.top_modal().is_none());
}

/// `/logout <provider>` reads the entry's keys first; one key keeps the confirmation and `y`
/// removes the whole entry (ADR 0026 D3).
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
    assert_eq!(
        effects,
        vec![Effect::LogoutCheck {
            provider: "corp".to_owned()
        }],
        "the host reads the keys first"
    );
    app.update(Input::LogoutChoices {
        provider: "corp".to_owned(),
        choices: Ok(vec![radian_host::KeyChoice {
            name: "first".to_owned(),
            masked: "sk-…irst".to_owned(),
        }]),
    });
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
    assert_eq!(
        submit(&mut app, "/logout corp"),
        vec![Effect::LogoutCheck {
            provider: "corp".to_owned()
        }]
    );
    app.update(Input::LogoutChoices {
        provider: "corp".to_owned(),
        choices: Ok(vec![radian_host::KeyChoice {
            name: "first".to_owned(),
            masked: "sk-…irst".to_owned(),
        }]),
    });
    let effects = app.on_key(key(KeyCode::Char('y'))).effects;
    assert_eq!(
        effects,
        vec![Effect::Logout {
            provider: "corp".to_owned(),
            key: None,
        }]
    );
    assert!(app.pending_logout().is_none());
    // The removal result names the provider either way.
    app.update(Input::LogoutDone {
        provider: "corp".to_owned(),
        key: None,
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

/// ADR 0026 D3: several keys are listed (each named and masked, plus `all`); Enter removes the
/// choice, `a` removes every key, and a named `/logout <provider> <name>` skips the question.
#[test]
fn logout_lists_several_keys_and_removes_the_chosen_one() {
    let mut app = App::new("m", "s");
    assert_eq!(
        submit(&mut app, "/logout opencode-go"),
        vec![Effect::LogoutCheck {
            provider: "opencode-go".to_owned()
        }]
    );
    app.update(Input::LogoutChoices {
        provider: "opencode-go".to_owned(),
        choices: Ok(vec![
            radian_host::KeyChoice {
                name: "first".to_owned(),
                masked: "sk-…irst".to_owned(),
            },
            radian_host::KeyChoice {
                name: "team".to_owned(),
                masked: "sk-…cond".to_owned(),
            },
        ]),
    });
    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("╭─ /logout "), "{screen}");
    assert!(screen.contains("1 first (sk-…irst)"), "{screen}");
    assert!(screen.contains("2 team (sk-…cond)"), "{screen}");
    assert!(screen.contains("all keys"), "{screen}");

    // Down picks `team`; Enter removes exactly it.
    app.on_key(key(KeyCode::Down));
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(
        effects,
        vec![Effect::Logout {
            provider: "opencode-go".to_owned(),
            key: Some("team".to_owned()),
        }]
    );
    assert!(app.logout_keys_mut().is_none());

    // `a` removes every key, whatever the cursor.
    let mut app = App::new("m", "s");
    submit(&mut app, "/logout opencode-go");
    app.update(Input::LogoutChoices {
        provider: "opencode-go".to_owned(),
        choices: Ok(vec![
            radian_host::KeyChoice {
                name: "first".to_owned(),
                masked: "sk-…irst".to_owned(),
            },
            radian_host::KeyChoice {
                name: "team".to_owned(),
                masked: "sk-…cond".to_owned(),
            },
        ]),
    });
    let effects = app.on_key(key(KeyCode::Char('a'))).effects;
    assert_eq!(
        effects,
        vec![Effect::Logout {
            provider: "opencode-go".to_owned(),
            key: None,
        }]
    );

    // A named key is removed without asking.
    let mut app = App::new("m", "s");
    assert_eq!(
        submit(&mut app, "/logout opencode-go team"),
        vec![Effect::Logout {
            provider: "opencode-go".to_owned(),
            key: Some("team".to_owned()),
        }]
    );
    assert!(app.top_modal().is_none());
}

/// `/login`'s first level completes the built-in descriptors plus `custom` — no host needed
/// (D3/D10).
#[test]
fn login_completion_lists_the_builtins_and_custom() {
    use radian_tui::command::{self, Level};
    let tree = command::tree();
    let level = tree
        .iter()
        .find(|command| command.name == "login")
        .map(|command| command.levels[0])
        .expect("login is in the tree");
    match level(&[], "") {
        Level::Ready(candidates) => {
            let ids: Vec<&str> = candidates
                .iter()
                .map(|candidate| candidate.insert.as_str())
                .collect();
            assert_eq!(
                ids,
                [
                    "opencode-go",
                    "anthropic",
                    "openai",
                    "openai-compatible",
                    "chatgpt",
                    "chatgpt-device",
                    "custom"
                ]
            );
        }
        other => panic!("expected Ready, got {other:?}"),
    }
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

/// `/login chatgpt` (ADR 0026) opens the browser-waiting step and returns
/// `Effect::LoginOAuthBrowser` directly — no key is ever asked for. The loop's reported URL
/// lands in the wizard; it is dropped once the wizard has moved on.
#[test]
fn login_chatgpt_begins_the_browser_flow_and_the_url_lands_in_the_wizard() {
    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/login chatgpt");
    assert_eq!(effects, vec![Effect::LoginOAuthBrowser]);
    assert!(matches!(
        top_login(&app).step,
        LoginStep::OAuthBrowserWaiting { url: None }
    ));

    app.update(Input::LoginOAuthUrlReady {
        url: "https://auth.openai.com/oauth/authorize?client_id=…".to_owned(),
    });
    match &top_login(&app).step {
        LoginStep::OAuthBrowserWaiting { url: Some(u) } => {
            assert!(u.starts_with("https://auth.openai.com/"), "{u}");
        }
        other => panic!("{other:?}"),
    }

    // Esc cancels the wizard; a URL reported afterwards (the background task finishing its
    // bind) is a no-op — there is no modal to update.
    app.update(Input::LoginKey(LoginKey::Esc));
    assert!(app.top_modal().is_none());
    app.update(Input::LoginOAuthUrlReady {
        url: "https://late.example".to_owned(),
    });
    assert!(app.top_modal().is_none());
}

/// `/login chatgpt-device` opens the device-waiting step and returns `Effect::LoginOAuthDevice`
/// directly; the loop's reported code lands in the wizard.
#[test]
fn login_chatgpt_device_begins_the_device_flow_and_the_code_lands_in_the_wizard() {
    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/login chatgpt-device");
    assert_eq!(effects, vec![Effect::LoginOAuthDevice]);
    assert!(matches!(
        top_login(&app).step,
        LoginStep::OAuthDeviceWaiting { .. }
    ));

    app.update(Input::LoginOAuthDeviceReady {
        user_code: "ABCD-1234".to_owned(),
        verification_url: "https://auth.openai.com/codex/device".to_owned(),
    });
    match &top_login(&app).step {
        LoginStep::OAuthDeviceWaiting {
            user_code,
            verification_url,
        } => {
            assert_eq!(user_code, "ABCD-1234");
            assert_eq!(verification_url, "https://auth.openai.com/codex/device");
        }
        other => panic!("{other:?}"),
    }
}

/// Either ChatGPT flow ends on `Input::LoginSaved` (not a separate event) — the wizard closes
/// and the notice names the provider, exactly like the API-key path.
#[test]
fn a_chatgpt_oauth_flow_closes_on_login_saved_like_the_api_key_path() {
    let mut app = App::new("m", "s");
    assert_eq!(
        submit(&mut app, "/login chatgpt"),
        vec![Effect::LoginOAuthBrowser]
    );
    app.update(Input::LoginSaved {
        name: "chatgpt".to_owned(),
        result: Ok(()),
    });
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app).iter().any(|n| n.contains("chatgpt")),
        "{:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    assert_eq!(
        submit(&mut app, "/login chatgpt-device"),
        vec![Effect::LoginOAuthDevice]
    );
    app.update(Input::LoginSaved {
        name: "chatgpt".to_owned(),
        result: Err("port 1455 is already in use".to_owned()),
    });
    assert!(app.top_modal().is_none());
    assert!(
        notices(&app).iter().any(|n| n.contains("port 1455")),
        "{:?}",
        notices(&app)
    );
}
