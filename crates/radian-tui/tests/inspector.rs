//! The inspector overlay (`jobs | tools | mcp | hooks`): parsing, tabs, selection,
//! detail, the jobs page's `k` confirm, and event-driven refreshes. Pure `update` tests;
//! the pty tests keep the terminal state and the idle zero-redraw rule.

use radian_tui::app::{App, Effect, Input};
use radian_tui::command::InspectorPage;
use radian_tui::inspector::{Inspector, InspectorSnapshot};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn snapshot() -> InspectorSnapshot {
    InspectorSnapshot {
        jobs: vec![radian_tui::inspector::JobEntry {
            id: "job-1".to_owned(),
            command: "sleep 60".to_owned(),
            state_label: "running".to_owned(),
            elapsed_secs: 7,
            running: true,
            output_tail: "out\n".to_owned(),
        }],
        tools: vec![radian_tui::inspector::ToolEntry {
            name: "read".to_owned(),
            description: "read a file".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
            bytes: 100,
            tokens: 25,
            source: "builtin".to_owned(),
        }],
        mcp: vec![radian_tui::inspector::McpEntry {
            id: "server".to_owned(),
            source: "--mcp-config mcp.json".to_owned(),
            state: "running".to_owned(),
            tools: 1,
            truncated: "—".to_owned(),
        }],
        hooks: vec![radian_tui::inspector::HookEntry {
            event: "PreToolUse".to_owned(),
            matcher: "shell".to_owned(),
            command: "echo hi".to_owned(),
            timeout_secs: 60,
            source: "user config.toml".to_owned(),
            level: "user".to_owned(),
            effective: true,
        }],
        hook_runs: Vec::new(),
        mcp_config_path: Some("--mcp-config mcp.json".to_owned()),
    }
}

fn open(app: &mut App, page: Option<InspectorPage>) {
    let result = app.update(Input::InspectorOpened {
        requested: page,
        snapshot: snapshot(),
    });
    assert!(result.dirty);
    assert!(app.inspector().is_some());
}

/// The four commands open their page; `/panel` opens the last page.
#[test]
fn the_four_commands_open_their_page_and_panel_returns_to_the_last() {
    let mut app = App::new("m", "s");
    // No last page yet: `/panel` opens jobs.
    open(&mut app, None);
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Jobs);
    // Switch to tools; the last page follows.
    app.update(Input::InspectorKey(key(KeyCode::Right)));
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Tools);
    // Esc closes; `/panel` returns to tools, not jobs.
    app.update(Input::InspectorKey(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(app.inspector().is_none());
    open(&mut app, None);
    assert_eq!(
        app.inspector().unwrap().page,
        InspectorPage::Tools,
        "/panel ignores the last page"
    );
    // An explicit page always wins over the last one.
    open(&mut app, Some(InspectorPage::Mcp));
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Mcp);
    open(&mut app, Some(InspectorPage::Hooks));
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Hooks);
}

/// Tab order follows the tab row; a switch resets the selection and the detail.
#[test]
fn tab_cycles_and_resets_selection_and_detail() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Jobs));
    app.update(Input::InspectorKey(key(KeyCode::Down)));
    app.update(Input::InspectorKey(key(KeyCode::Enter)));
    assert!(app.inspector().unwrap().detail);
    app.update(Input::InspectorKey(key(KeyCode::Tab)));
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Tools);
    assert_eq!(app.inspector().unwrap().selected, 0);
    assert!(!app.inspector().unwrap().detail);
}

/// Esc closes to the composer; the input is kept.
#[test]
fn esc_closes_to_the_composer() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Jobs));
    app.input = "hello".into();
    app.update(Input::InspectorKey(KeyEvent::new(
        KeyCode::Esc,
        KeyModifiers::NONE,
    )));
    assert!(app.inspector().is_none());
    assert_eq!(app.input.text(), "hello");
}

/// `k` arms the confirm; it never kills directly. `y` kills through the same path as
/// `/jobs kill`, `n` cancels.
#[test]
fn k_arms_a_confirm_and_y_kills_through_the_jobs_path() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Jobs));
    // `k` arms, it does not kill: no effect, just the confirm.
    let result = app.update(Input::InspectorKey(key(KeyCode::Char('k'))));
    assert!(result.effects.is_empty(), "k kills without the confirm");
    assert_eq!(
        app.inspector().unwrap().pending_kill.as_deref(),
        Some("job-1")
    );
    // `y` kills through the `/jobs kill` path.
    let result = app.update(Input::InspectorKey(key(KeyCode::Char('y'))));
    assert_eq!(
        result.effects,
        vec![Effect::Jobs(radian_tui::command::JobsRequest::Kill(
            "job-1".to_owned()
        ))]
    );
    // Re-arm, then `n` cancels.
    app.update(Input::InspectorKey(key(KeyCode::Char('k'))));
    let result = app.update(Input::InspectorKey(key(KeyCode::Char('n'))));
    assert!(result.effects.is_empty());
    assert!(app.inspector().unwrap().pending_kill.is_none());
}

/// `k` is jobs-only: another page offers no id and produces no effect.
#[test]
fn k_outside_jobs_does_nothing() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Tools));
    let result = app.update(Input::InspectorKey(key(KeyCode::Char('k'))));
    assert!(result.effects.is_empty());
    assert!(app.inspector().unwrap().pending_kill.is_none());
}

/// A refresh keeps the page and clamps the selection; a pending `k` for a gone job is
/// dropped.
#[test]
fn a_refresh_keeps_the_page_and_drops_a_gone_confirm() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Jobs));
    app.update(Input::InspectorKey(key(KeyCode::Char('k'))));
    assert!(app.inspector().unwrap().pending_kill.is_some());
    // The job is gone: the selection clamps and the confirm drops.
    let empty = InspectorSnapshot::default();
    app.update(Input::InspectorRefreshed(empty));
    assert_eq!(app.inspector().unwrap().page, InspectorPage::Jobs);
    assert_eq!(app.inspector().unwrap().selected, 0);
    assert!(app.inspector().unwrap().pending_kill.is_none());
}

/// A job settle while the panel is open asks for a refresh; without the panel it only
/// redraws. No polling: the refresh is requested by the event, never by a timer.
#[test]
fn a_job_settle_refreshes_only_while_open() {
    use radian_session::RuntimeEvent;
    let mut app = App::new("m", "s");
    let settled = RuntimeEvent::BackgroundJobSettled {
        job_id: "job-1".to_owned(),
        state: "exited 0".to_owned(),
        duration_ms: 100,
    };
    let result = app.update(Input::Runtime(settled.clone()));
    assert!(
        result.effects.is_empty(),
        "no panel open: no refresh effect"
    );
    open(&mut app, Some(InspectorPage::Jobs));
    let result = app.update(Input::Runtime(settled));
    assert_eq!(result.effects, vec![Effect::RefreshInspector]);
}

/// A job settle with the panel open and `tui.bell = all` yields both effects: the
/// inspector refresh first, then the bell.
#[test]
fn a_job_settle_with_the_panel_open_and_bell_all_refreshes_and_rings() {
    use radian_session::RuntimeEvent;
    let mut app = App::new("m", "s");
    app.bell = radian_tui::bell::BellMode::All;
    open(&mut app, Some(InspectorPage::Jobs));
    let result = app.update(Input::Runtime(RuntimeEvent::BackgroundJobSettled {
        job_id: "job-1".to_owned(),
        state: "exited 0".to_owned(),
        duration_ms: 100,
    }));
    assert!(result.dirty);
    assert_eq!(result.effects, vec![Effect::RefreshInspector, Effect::Bell]);
}

/// The inspector is an overlay, never a modal: it opens below a modal.
#[test]
fn the_inspector_stays_below_a_modal() {
    let mut app = App::new("m", "s");
    open(&mut app, Some(InspectorPage::Jobs));
    app.push_modal(radian_tui::app::Modal::ConfirmLogout("p".to_owned()));
    assert!(app.inspector().is_some());
    assert!(app.top_modal().is_some());
    let tiers: Vec<_> = app.layers().iter().map(|layer| layer.tier()).collect();
    assert_eq!(
        tiers,
        vec![
            radian_tui::app::Tier::Surface,
            radian_tui::app::Tier::Overlay,
            radian_tui::app::Tier::Modal
        ]
    );
}

/// The overlay state itself: switching pages resets, like the app-level test pins.
#[test]
fn inspector_state_switch_resets() {
    let mut inspector = Inspector::new(InspectorPage::Jobs, snapshot());
    inspector.selected = 5;
    inspector.detail = true;
    inspector.next_page();
    assert_eq!(inspector.page, InspectorPage::Tools);
    assert_eq!(inspector.selected, 0);
}
