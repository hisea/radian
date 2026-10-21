//! The `tui.theme` and `tui.keybindings` wiring (ADR 0008 §2, ADR 0009): applying a config to
//! `App`, the theme screenshot, and the rebind/conflict rules.

use radian_permissions::ApprovalOutcome;
use radian_tui::app::{App, Effect, Input};
use radian_tui::theme::ThemeName;
use radian_tui::{render, theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

fn config(text: &str) -> radian_host::config::Config {
    radian_host::config::parse(std::path::Path::new("/tmp/config.toml"), text).unwrap()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

#[test]
fn a_configured_theme_is_applied_and_no_color_forces_mono() {
    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui]\ntheme = \"light\"\n"),
        false,
    )
    .unwrap();
    assert_eq!(app.theme.name, ThemeName::Light);

    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui]\ntheme = \"light\"\n"),
        true,
    )
    .unwrap();
    assert_eq!(app.theme.name, ThemeName::Mono, "NO_COLOR wins");

    // An unknown theme is ignored (the reader reports it), leaving the default.
    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui]\ntheme = \"future\"\n"),
        false,
    )
    .unwrap();
    assert_eq!(app.theme.name, ThemeName::Dark);
}

#[test]
fn a_rebound_send_key_sends_and_enter_becomes_a_no_op() {
    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-s\"]\n"),
        false,
    )
    .unwrap();
    app.input.set_text("hello");
    assert!(
        app.on_key(key(KeyCode::Enter)).effects.is_empty(),
        "Enter no longer sends"
    );
    assert_eq!(app.input, "hello", "the text is still there");
    assert_eq!(
        app.on_key(ctrl(KeyCode::Char('s'))).effects,
        vec![Effect::ExpandMentions {
            text: "hello".into()
        }]
    );
}

#[test]
fn a_keymap_conflict_is_a_startup_error() {
    let mut app = App::new("m", "s");
    let error = app
        .apply_config(
            &config("schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-j\"]\n"),
            false,
        )
        .unwrap_err();
    assert!(error.contains("ctrl-j"), "{error}");
    assert!(
        error.contains("send") && error.contains("newline"),
        "{error}"
    );
}

/// A printable single character bound in the main view would swallow that
/// character in the composer, so the config application returns a warning for it (the binding is
/// kept; only the warning is added). A Ctrl/Alt binding is not a typed character, so it is silent.
#[test]
fn a_printable_key_in_the_main_view_is_warned_about() {
    for (bindings, warned) in [
        ("toggle_reasoning = [\"p\"]\n", true),
        ("toggle_reasoning = [\"ctrl-y\"]\n", false),
    ] {
        let text = format!("schema_version = 1\n[tui.keybindings]\n{bindings}");
        let mut app = App::new("m", "s");
        let warnings = app.apply_config(&config(&text), false).unwrap();
        let found: Vec<&String> = warnings
            .iter()
            .filter(|warning| warning.contains("cannot be typed in the input box"))
            .collect();
        assert_eq!(found.len(), usize::from(warned), "{bindings}: {warnings:?}");
        if warned {
            assert!(
                found[0].contains("'p'") && found[0].contains("toggle_reasoning"),
                "{found:?}"
            );
        }
    }
}

#[test]
fn the_approval_keys_cannot_be_taken_over_by_the_keymap() {
    // The config binds `y` to cancel; during a prompt `y` still means "allow once".
    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui.keybindings]\ncancel = [\"y\"]\n"),
        false,
    )
    .unwrap();
    app.update(Input::Pending(
        Some(Box::new(radian_host::PendingView {
            request_id: 1,
            origin: "session".into(),
            request: radian_host::PendingRequest::Approval(
                radian_permissions::ApprovalRequest::new(
                    "perm-1",
                    "call-1",
                    "write",
                    "/ws/out.txt",
                    "/ws/out.txt",
                    serde_json::json!({}),
                    None,
                ),
            ),
        })),
        1,
    ));
    // The hardcoded prompt key wins over the keymap: it answers, it does not cancel.
    assert_eq!(
        app.on_key(key(KeyCode::Char('y'))).effects,
        vec![Effect::AnswerRequest {
            request_id: 1,
            response: radian_host::PendingResponse::Approval(ApprovalOutcome::AllowOnce),
        }]
    );
    assert!(!app.quit, "the prompt key did not cancel/quit");
}

/// The find box is a layer, so its keys are rebindable without touching the transcript.
#[test]
fn the_find_layer_is_rebindable() {
    let mut app = App::new("m", "s");
    app.push_message(&agent_core::Message::user("alpha"));
    app.input.set_text("draft");
    app.apply_config(
        &config(
            "schema_version = 1
[tui.keybindings]
find = [\"ctrl-s\"]
find_close = [\"ctrl-x\"]
",
        ),
        false,
    )
    .unwrap();
    // `find` moves to Ctrl-S: Ctrl-F no longer opens the box.
    app.on_key(ctrl(KeyCode::Char('f')));
    assert!(app.find_state().is_none(), "Ctrl-F is no longer `find`");
    app.on_key(ctrl(KeyCode::Char('s')));
    assert!(app.find_state().is_some(), "Ctrl-S opened the box");
    // `find_close` moves to Ctrl-X; Esc falls through to the transcript's `escape`, which
    // clears the draft (and does not close the box or quit).
    app.on_key(key(KeyCode::Esc));
    assert!(app.find_state().is_some(), "Esc is no longer find_close");
    assert!(!app.quit, "Esc cleared the input instead of quitting");
    assert_eq!(app.input, "", "the draft was cleared");
    app.on_key(ctrl(KeyCode::Char('x')));
    assert!(app.find_state().is_none(), "Ctrl-X closed the box");
}

/// `tui.input_style`: `lines` and `box` are applied; the removed `bar` and any unknown value
/// warn once and fall back to `lines` (ADR 0035's 2026-10-16 note; the `tui.*` rule).
#[test]
fn the_input_style_is_applied_and_an_unknown_value_falls_back_to_lines() {
    // ADR 0035: `lines` is the default.
    let mut app = App::new("m", "s");
    let warnings = app
        .apply_config(
            &config("schema_version = 1\n[tui]\ninput_style = \"future\"\n"),
            false,
        )
        .unwrap();
    assert_eq!(
        app.input_style,
        radian_tui::render::InputStyle::Lines,
        "an unknown style falls back to the default"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("tui.input_style")),
        "{warnings:?}"
    );

    // The removed `bar` warns once with its own message and uses `lines`.
    let mut app = App::new("m", "s");
    let warnings = app
        .apply_config(
            &config("schema_version = 1\n[tui]\ninput_style = \"bar\"\n"),
            false,
        )
        .unwrap();
    assert_eq!(
        app.input_style,
        radian_tui::render::InputStyle::Lines,
        "the removed bar falls back to lines"
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning == "tui.input_style = \"bar\" was removed; using lines"),
        "{warnings:?}"
    );

    for (spelling, expected) in [
        ("box", radian_tui::render::InputStyle::Box),
        ("lines", radian_tui::render::InputStyle::Lines),
    ] {
        let mut app = App::new("m", "s");
        app.apply_config(
            &config(&format!(
                "schema_version = 1\n[tui]\ninput_style = \"{spelling}\"\n"
            )),
            false,
        )
        .unwrap();
        assert_eq!(app.input_style, expected, "{spelling}");
    }
}

/// The `dark` theme is the pre-config palette; this is the screenshot assertion for a couple
/// of key elements.
#[test]
fn the_dark_theme_keeps_the_original_colors() {
    let mut app = App::new("m", "s");
    app.push_message(&agent_core::Message::user("hello"));
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut user_marker = None;
    for y in 0..12 {
        let mut line = String::new();
        for x in 0..40 {
            line.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        if let Some(col) = line.find("❯ ") {
            user_marker = buffer.cell((col as u16, y)).map(|c| c.style());
            break;
        }
    }
    let style = user_marker.expect("the user marker");
    // The built-in dark palette is pi's. ADR 0029: the user marker is
    // the accent foreground now (the `user` role), not the main text level.
    assert_eq!(style.fg, Some(Color::Rgb(167, 152, 215)));
}

/// `mono` renders with no color at all.
#[test]
fn the_mono_theme_renders_without_color() {
    let mut app = App::new("m", "s");
    app.theme = theme::Theme::mono();
    app.push_message(&agent_core::Message::user("hello"));
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    for cell in buffer.content() {
        // `Color::Reset` is the TestBackend's "no color" (what `Style::default()` leaves).
        assert_eq!(cell.fg, Color::Reset, "{cell:?}");
        assert_eq!(cell.bg, Color::Reset, "{cell:?}");
    }
}
