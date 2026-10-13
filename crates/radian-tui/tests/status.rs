//! The status bar: the permission tier, provider-reported usage, the MCP state, and the
//! narrow-terminal truncation rule. Pure `App` state on a `TestBackend`, so it needs no pty.

use agent_core::{AgentEvent, ModelUsage, StopReason};
use radian_tui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

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

fn response(input: Option<u64>, output: Option<u64>) -> AgentEvent {
    AgentEvent::ModelResponseReceived {
        stop_reason: StopReason::EndTurn,
        tool_calls: 0,
        usage: Some(ModelUsage {
            input,
            output,
            ..Default::default()
        }),
    }
}

#[test]
fn the_status_bar_shows_the_tier_reported_usage_and_mcp_counts() {
    let mut app = App::new("mock", "s");
    app.set_tier("workspace");
    app.set_mcp(1, Some((1, 15)));
    app.apply_event(&response(Some(1200), Some(340)));

    let text = screen(&mut app, 140, 12);
    assert!(text.contains("workspace"), "{text}");
    assert!(text.contains("usage 1200/340"), "{text}");
    assert!(text.contains("mcp 1/15"), "{text}");
    assert!(
        text.contains("context guard off (no window configured)"),
        "the budget segment is still there when it fits: {text}"
    );
}

#[test]
fn a_session_without_reports_says_none_and_never_invents_a_zero() {
    let mut app = App::new("mock", "s");
    app.set_tier("safe");
    app.set_mcp(0, None);
    // One response with no usage at all, then one that reports only input.
    app.apply_event(&response(None, None));
    app.apply_event(&response(Some(10), None));

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
    let text = screen(&mut fresh, 140, 12);
    assert!(text.contains("usage none"), "{text}");
    assert!(!text.contains("usage 0"), "{text}");
    assert!(text.contains("mcp off"), "{text}");
}

#[test]
fn a_narrow_status_bar_keeps_the_key_segments_and_does_not_wrap() {
    let mut app = App::new("mock", "s");
    app.set_tier("workspace");
    app.set_mcp(1, Some((1, 15)));
    app.apply_event(&response(Some(1200), Some(340)));

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
