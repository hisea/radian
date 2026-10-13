//! The `tui.theme` and `tui.keybindings` wiring (ADR 0008 §2, ADR 0009): applying a config to
//! `App`, the theme screenshot, and the rebind/conflict rules.

use radian_permissions::ApprovalOutcome;
use radian_tui::app::{App, ApprovalPrompt, Send};
use radian_tui::theme::ThemeName;
use radian_tui::{render, theme};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier};

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
    assert_eq!(
        app.on_key(key(KeyCode::Enter)),
        None,
        "Enter no longer sends"
    );
    assert_eq!(app.input, "hello", "the text is still there");
    assert_eq!(
        app.on_key(ctrl(KeyCode::Char('s'))),
        Some(Send::Followup("hello".into()))
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

#[test]
fn the_approval_keys_cannot_be_taken_over_by_the_keymap() {
    // The config binds `y` to cancel; during a prompt `y` still means "allow once".
    let mut app = App::new("m", "s");
    app.apply_config(
        &config("schema_version = 1\n[tui.keybindings]\ncancel = [\"y\"]\n"),
        false,
    )
    .unwrap();
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    app.request_approval(ApprovalPrompt {
        request: radian_permissions::ApprovalRequest::new(
            "perm-1",
            "call-1",
            "write",
            "/ws/out.txt",
            "/ws/out.txt",
            serde_json::json!({}),
            None,
        ),
        reply,
    });
    assert_eq!(app.on_key(key(KeyCode::Char('y'))), None);
    assert!(!app.quit, "the prompt key did not cancel/quit");
    assert!(matches!(answer.try_recv(), Ok(ApprovalOutcome::AllowOnce)));
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
        if let Some(col) = line.find("> ") {
            user_marker = buffer.cell((col as u16, y)).map(|c| c.style());
            break;
        }
    }
    let style = user_marker.expect("the user marker");
    assert_eq!(style.fg, Some(Color::Cyan));
    assert!(style.add_modifier.contains(Modifier::BOLD));
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
