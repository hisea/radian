//! Scroll-follow logic: new content follows while the view is at the bottom, and never
//! drags the user back after they scroll up.

use agent_core::AgentEvent;
use radian_tui::app::App;
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn render(app: &mut App) {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
}

fn fill(app: &mut App, n: usize) {
    for i in 0..n {
        app.apply_event(&AgentEvent::TranscriptAppended {
            index: i,
            message: agent_core::Message::user(format!("line {i}")),
        });
    }
}

#[test]
fn new_content_follows_while_at_the_bottom() {
    let mut app = App::new("m", "s");
    fill(&mut app, 40);
    render(&mut app);
    assert!(app.is_following());
    assert_eq!(app.scroll, app.max_scroll);
    assert!(app.max_scroll > 0, "the transcript must overflow the view");

    // New content arrives: still pinned to the bottom.
    app.apply_event(&AgentEvent::ModelTextDelta {
        text: "more".into(),
    });
    render(&mut app);
    assert!(app.is_following());
    assert_eq!(app.scroll, app.max_scroll);
}

#[test]
fn scrolling_up_pauses_following_and_new_content_does_not_drag_back() {
    let mut app = App::new("m", "s");
    fill(&mut app, 40);
    render(&mut app);
    let bottom = app.max_scroll;

    // Page up.
    app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    render(&mut app);
    assert!(!app.is_following(), "scrolling up pauses follow");
    let paused_at = app.scroll;
    assert!(paused_at < bottom);

    // New content arrives while paused: the viewport must stay on the same lines.
    for i in 0..5 {
        app.apply_event(&AgentEvent::TranscriptAppended {
            index: 100 + i,
            message: agent_core::Message::user(format!("new {i}")),
        });
    }
    render(&mut app);
    assert!(!app.is_following(), "new content must not un-pause");
    assert_eq!(app.scroll, paused_at, "the view must not be dragged down");
    assert!(app.max_scroll > bottom, "new lines were appended");
}

#[test]
fn scrolling_back_to_the_bottom_resumes_following() {
    let mut app = App::new("m", "s");
    fill(&mut app, 40);
    render(&mut app);
    app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    render(&mut app);
    assert!(!app.is_following());

    app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    render(&mut app);
    assert!(app.is_following());
    assert_eq!(app.scroll, app.max_scroll);

    app.apply_event(&AgentEvent::ModelTextDelta { text: "x".into() });
    render(&mut app);
    assert!(app.is_following());
    assert_eq!(app.scroll, app.max_scroll);
}

#[test]
fn page_down_stops_at_the_bottom_and_resumes_following() {
    let mut app = App::new("m", "s");
    fill(&mut app, 40);
    render(&mut app);
    for _ in 0..10 {
        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    }
    render(&mut app);
    assert_eq!(app.scroll, 0, "page up stops at the top");
    assert!(!app.is_following());

    for _ in 0..10 {
        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
    }
    render(&mut app);
    assert_eq!(app.scroll, app.max_scroll);
    assert!(app.is_following(), "reaching the bottom resumes follow");
}
