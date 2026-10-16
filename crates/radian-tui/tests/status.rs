//! The status bar: the permission tier, provider-reported usage, the MCP state, and the
//! narrow-terminal truncation rule. Pure `App` state on a `TestBackend`, so it needs no pty.

use agent_core::ModelUsage;
use radian_tui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Puts a test on the pre-ADR one-line status bar (`preset = classic`; ADR 0027 C2). These
/// tests assert the classic composition; the new default is the two-row `pi` footer.
fn classic_footer(app: &mut App) {
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        preset: Some("classic".to_string()),
        ..Default::default()
    });
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

/// Records one provider-reported usage the way the event sink does (ADR 0023 U5): the fact is
/// written to the session projection, not to `App`.
fn record_usage(app: &App, input: Option<u64>, output: Option<u64>) {
    app.session.projection.add_usage(&ModelUsage {
        input,
        output,
        ..Default::default()
    });
}

#[test]
fn the_status_bar_shows_the_tier_reported_usage_and_mcp_counts() {
    let mut app = App::new("mock", "s");
    classic_footer(&mut app);
    app.set_tier("workspace");
    app.set_mcp(1, Some((1, 15)));
    record_usage(&app, Some(1200), Some(340));

    let text = screen(&mut app, 140, 12);
    assert!(text.contains("workspace"), "{text}");
    assert!(text.contains("usage 1200/340"), "{text}");
    assert!(text.contains("mcp 1/15"), "{text}");
    assert!(
        text.contains("context budget off (no window configured)"),
        "the budget segment is still there when it fits: {text}"
    );
}

#[test]
fn a_session_without_reports_says_none_and_never_invents_a_zero() {
    let mut app = App::new("mock", "s");
    classic_footer(&mut app);
    app.set_tier("safe");
    app.set_mcp(0, None);
    // One response with no usage at all, then one that reports only input. A response without
    // usage writes no fact at all.
    record_usage(&app, None, None);
    record_usage(&app, Some(10), None);

    let text = screen(&mut app, 140, 12);
    assert!(
        text.contains("usage 10/-"),
        "only the reported field may appear: {text}"
    );
    assert!(
        !text.contains("usage 0"),
        "an unreported field is never shown as zero: {text}"
    );

    let mut fresh = App::new("mock", "s");
    classic_footer(&mut fresh);
    let text = screen(&mut fresh, 140, 12);
    assert!(text.contains("usage none"), "{text}");
    assert!(!text.contains("usage 0"), "{text}");
    assert!(text.contains("mcp off"), "{text}");
}

#[test]
fn a_narrow_status_bar_keeps_the_key_segments_and_does_not_wrap() {
    let mut app = App::new("mock", "s");
    classic_footer(&mut app);
    app.set_tier("workspace");
    app.set_mcp(1, Some((1, 15)));
    record_usage(&app, Some(1200), Some(340));

    let text = screen(&mut app, 60, 12);
    for needle in ["mock", "workspace", "usage 1200/340", "mcp 1/15"] {
        assert!(
            text.contains(needle),
            "60 columns dropped {needle:?}: {text}"
        );
    }
    // The long context text is the first to go, and its loss is marked.
    assert!(
        !text.contains("no window configured"),
        "the long segment should be the one dropped: {text}"
    );
    assert!(text.contains('…'), "truncation must be marked: {text}");
    // The status line is still one row; the frame has the requested height.
    assert_eq!(text.lines().count(), 12, "{text}");
}

/// The first idle Ctrl-C arms the quit window; the status bar says so until it is disarmed
///.
#[test]
fn the_status_bar_shows_the_ctrl_c_quit_hint_while_armed() {
    use std::time::Instant;
    let mut app = App::new("mock", "s");
    app.quit_pending = Some(Instant::now() + radian_tui::app::QUIT_WINDOW);
    let text = screen(&mut app, 120, 12);
    assert!(
        text.contains("Press Ctrl-C again to quit"),
        "the hint must be visible: {text}"
    );

    app.quit_pending = None;
    let text = screen(&mut app, 120, 12);
    assert!(!text.contains("Press Ctrl-C again to quit"), "{text}");
}

/// The foreground color of the first cell of the first row containing `needle`.
fn fg_of(app: &mut App, width: u16, height: u16, needle: &str) -> Option<ratatui::style::Color> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let area = *buffer.area();
    let mut text = String::new();
    let mut cells: Vec<u16> = Vec::new();
    for y in area.top()..area.bottom() {
        text.clear();
        cells.clear();
        for x in area.left()..area.right() {
            let symbol = buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
            text.push_str(symbol);
            for _ in 0..symbol.len() {
                cells.push(x);
            }
        }
        if let Some(byte) = text.find(needle) {
            return cells
                .get(byte)
                .and_then(|x| buffer.cell((*x, y)))
                .map(|c| c.fg);
        }
    }
    None
}

/// `/theme` applies immediately: the same cell renders with the new theme's color (owner
/// 2026-10-14). ADR 0029 re-picked the built-in palettes, so the assertion is
/// "the light theme's own model color", not a hard-coded ANSI name.
#[test]
fn applying_a_theme_changes_the_rendered_colors() {
    let mut app = App::new("mock", "s");
    let dark = fg_of(&mut app, 120, 12, "mock").expect("the model segment");
    assert_eq!(
        dark,
        radian_tui::theme::Theme::dark()
            .model
            .fg
            .expect("dark model fg"),
        "dark's model color"
    );

    let result = app.update(radian_tui::app::Input::ThemeReady {
        theme: Some(Box::new(radian_tui::theme::Theme::light())),
        notices: vec!["theme saved to /tmp/config.toml".to_string()],
    });
    assert!(result.dirty);
    let light = fg_of(&mut app, 120, 12, "mock").expect("the model segment");
    assert_eq!(
        light,
        radian_tui::theme::Theme::light()
            .model
            .fg
            .expect("light model fg"),
        "light's model color"
    );
    assert_ne!(dark, light, "the switch must change the rendered color");
    assert!(
        app.session
            .entries
            .iter()
            .any(|entry| entry.text.contains("theme saved to /tmp/config.toml")),
        "the notice is shown"
    );
}
