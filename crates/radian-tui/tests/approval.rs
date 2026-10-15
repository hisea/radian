//! The approval dialog: what it shows, how keys answer it, and how the pending queue drives
//! it (ADR 0023 U7). Pure `App` state plus a `TestBackend` frame, so it runs everywhere
//! without a terminal.

use agent_core::{AgentEvent, CancelReason, RunStatus};
use radian_host::{PendingRequest, PendingResponse, PendingView};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_tui::app::{App, Effect, Input};
use radian_tui::render;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn request() -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-1",
        "call-1",
        "write",
        "sub/../out.txt",
        "/ws/out.txt",
        json!({"path": "sub/../out.txt", "content": "hello"}),
        None,
    )
}

/// The queue's view of that request: the id the UI shows and answers (ADR 0023 U7).
fn view() -> PendingView<PendingRequest> {
    PendingView {
        request_id: 1,
        origin: "session".into(),
        request: PendingRequest::Approval(request()),
    }
}

/// Puts the queue's head on screen the way the loop does.
fn prompt(app: &mut App) {
    app.update(Input::Pending(Some(Box::new(view())), 1));
}

/// Answers the way the event loop does: the effect names the queue's request id and the
/// response, and the loop hands both to the facade. Returns what the effect named.
fn answer(app: &mut App, key: KeyEvent) -> (u64, ApprovalOutcome) {
    let result = app.on_key(key);
    assert_eq!(result.effects.len(), 1, "{:?}", result.effects);
    match result.effects[0].clone() {
        Effect::AnswerRequest {
            request_id,
            response,
        } => match response {
            PendingResponse::Approval(outcome) => (request_id, outcome),
            other => panic!("expected an approval response, got {other:?}"),
        },
        other => panic!("expected an answer request, got {other:?}"),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn the_prompt_shows_the_resolved_path_not_the_raw_one() {
    // The permission layer resolves `sub/../out.txt` before asking; the prompt must show that
    // form, so a `..` cannot make the target look like it stayed under `sub/`.
    let text = render::approval_text(&request());
    assert!(text.contains("/ws/out.txt"), "{text}");
    assert!(text.contains("tool: write"), "{text}");
    assert!(text.contains("[y] allow once"), "{text}");
    assert!(text.contains("[a] always this session"), "{text}");
    assert!(text.contains("[n] deny"), "{text}");
    // The path line is the resolved one, not the raw argument.
    let path_line = text
        .lines()
        .find(|line| line.starts_with("path:"))
        .expect("a path line");
    assert_eq!(path_line, "path: /ws/out.txt");
}

#[test]
fn keys_answer_the_prompt_with_the_queue_id() {
    for (code, expected) in [
        (KeyCode::Char('y'), ApprovalOutcome::AllowOnce),
        (KeyCode::Char('a'), ApprovalOutcome::AllowForSession),
        (KeyCode::Char('n'), ApprovalOutcome::Deny { reason: None }),
        (KeyCode::Esc, ApprovalOutcome::Deny { reason: None }),
    ] {
        let mut app = App::new("m", "s");
        prompt(&mut app);
        let (request_id, outcome) = answer(&mut app, key(code));
        assert_eq!(request_id, 1, "{code:?}");
        assert_eq!(outcome, expected, "{code:?}");
        assert!(
            app.pending_approval().is_none(),
            "the prompt is cleared after an answer"
        );
    }
}

#[test]
fn keys_do_not_type_while_a_prompt_is_up() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.input.is_empty(),
        "keys answered the prompt: {:?}",
        app.input
    );
}

/// ADR 0023 U7: the queue drives the modal, not the turn. A finished turn produces no effect
/// for the prompt; the head going away (the permission layer's waiter was cancelled) is what
/// removes it, so "the waiter gave up" and "the turn ended" are the same event.
#[test]
fn the_queue_head_drives_the_prompt_not_the_turn() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    let result = app.update(Input::Event(AgentEvent::RunFinished {
        status: RunStatus::Cancelled(CancelReason::Requested),
    }));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(
        app.pending_approval().is_some(),
        "the turn ending does not by itself clear the modal"
    );

    app.update(Input::Pending(None, 0));
    assert!(app.pending_approval().is_none(), "the queue's head is gone");
    assert_eq!(app.pending, 0);
}

#[test]
fn ctrl_c_while_a_prompt_is_up_cancels_instead_of_answering() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    app.projection.set_turn(radian_tui::TurnState::Running);
    let result = app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(result.effects, vec![Effect::Cancel]);
    assert!(
        app.pending_approval().is_some(),
        "Ctrl-C cancels the turn; the queue clears the prompt when the waiter goes"
    );
}

/// Two approvals in a row: the second does not supersede the first (ADR 0023 U7 replaced that
/// behavior). The queue hands the UI one head at a time.
#[test]
fn a_second_approval_does_not_supersede_the_first() {
    let mut app = App::new("m", "s");
    prompt(&mut app);
    let second = PendingView {
        request_id: 2,
        origin: "session".into(),
        request: PendingRequest::Approval(request()),
    };
    // The queue's head is still the first request (the second is behind it), so nothing moves.
    app.update(Input::Pending(Some(Box::new(view())), 2));
    assert_eq!(
        app.pending_approval().map(|r| r.request_id.as_str()),
        Some("perm-1")
    );
    assert_eq!(app.pending, 2, "the status bar counts the queue");

    // When the first is answered, the second becomes the head. Moving to it must not answer
    // (deny) the first: the queue already did, and a second answer would be a late one.
    let result = app.update(Input::Pending(Some(Box::new(second)), 1));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert!(app.pending_approval().is_some());
    assert_eq!(app.pending, 1);
}

#[test]
fn the_approval_box_is_rendered_over_the_input() {
    let mut app = App::new("mock", "s");
    prompt(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut app))
        .unwrap();
    let screen = terminal.backend().to_string();
    assert!(screen.contains("approve tool call?"), "{screen}");
    assert!(screen.contains("/ws/out.txt"), "{screen}");
    assert!(screen.contains("allow once"), "{screen}");
    assert!(
        !screen.contains("type a message"),
        "the input box is replaced by the prompt: {screen}"
    );
}

/// The shared panel shape (ADR 0035): a rounded frame, the title on the top border,
/// the hardcoded key hints in the bottom border — at 80 and 120 columns, and in the ascii tier.
#[test]
fn the_approval_panel_snapshot_at_80_120_and_ascii() {
    let mut app = App::new("mock", "s");
    prompt(&mut app);

    let screen = draw(&mut app, 80, 24);
    let top = panel_line(&screen, '╭').expect("a top border");
    let bottom = panel_line(&screen, '╰').expect("a bottom border");
    assert!(top.starts_with("╭─ approve tool call? "), "{top:?}");
    assert!(top.ends_with('╮'), "{top:?}");
    assert_eq!(top.chars().count(), 80, "{top:?}");
    assert!(
        bottom.starts_with("╰─ [y] allow once · [a] always this session · [n] deny"),
        "{bottom:?}"
    );
    assert_eq!(bottom.chars().count(), 80, "{bottom:?}");

    let screen = draw(&mut app, 120, 24);
    let bottom = panel_line(&screen, '╰').expect("a bottom border");
    assert!(
        bottom.starts_with(
            "╰─ [y] allow once · [a] always this session · [n] deny · [!] auto for this session"
        ),
        "{bottom:?}"
    );
    assert!(bottom.ends_with("─╯"), "{bottom:?}");
    assert_eq!(bottom.chars().count(), 120, "{bottom:?}");

    // The ascii tier falls back to `+ - |` (ADR 0035).
    app.apply_footer(&radian_host::config::TuiFooterConfig {
        glyphs: Some("ascii".into()),
        ..Default::default()
    });
    let screen = draw(&mut app, 80, 24);
    let top = panel_line(&screen, '+').expect("an ascii top border");
    assert!(top.starts_with("+- approve tool call? "), "{top:?}");
    assert!(top.ends_with('+'), "{top:?}");
    assert!(!screen.contains('╭'), "{screen}");
}

/// The first screen line whose first cell is `lead` (the panel's own line). `TestBackend`'s
/// `Display` wraps every row in quotes, so they are stripped first.
fn panel_line(screen: &str, lead: char) -> Option<String> {
    screen
        .lines()
        .map(|line| line.trim_matches('"').to_string())
        .find(|line| line.starts_with(lead))
}

// ---------- the box fits: keys always visible, long bodies scroll (approval-fit) ----------

/// A shell call whose command is `lines` lines long: the case the owner hit (a multi-line
/// python heredoc in a curl pipe pushed the key line out of the box).
fn shell_request(command: &str) -> ApprovalRequest {
    ApprovalRequest::new(
        "perm-2",
        "call-2",
        "shell",
        command,
        command,
        json!({"command": command, "cwd": "/ws"}),
        None,
    )
}

fn long_command(lines: usize) -> String {
    (0..lines)
        .map(|index| format!("echo line-{index}\n"))
        .collect()
}

fn draw(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal.backend().to_string()
}

fn shell_app(lines: usize) -> App {
    let mut app = App::new("mock", "s");
    app.update(Input::Pending(
        Some(Box::new(PendingView {
            request_id: 9,
            origin: "session".into(),
            request: PendingRequest::Approval(shell_request(&long_command(lines))),
        })),
        1,
    ));
    app
}

/// A 60-line command still shows the key line: it is the box's own last row, not the body's.
#[test]
fn a_sixty_line_command_keeps_the_key_line_visible() {
    let mut app = shell_app(60);
    let screen = draw(&mut app, 80, 24);
    assert!(screen.contains("allow once"), "{screen}");
    assert!(screen.contains("always this session"), "{screen}");
    assert!(screen.contains("deny"), "{screen}");
    // The body starts at the top, so the command's first line is on screen too.
    assert!(screen.contains("echo line-0"), "{screen}");
}

/// The window indicator names the visible rows, and `↓` moves the first visible line.
#[test]
fn the_window_indicator_moves_with_down() {
    let mut app = shell_app(60);
    let width = 80u16;
    let total = render::wrap(
        &render::approval_body(&shell_request(&long_command(60))),
        76,
    )
    .len();
    let screen = draw(&mut app, width, 24);
    // `approval_cap(24, 0)` is 14 rows, so the panel shows 14 - 2 = 12 body rows.
    assert!(
        screen.contains(&format!("lines 1–12 of {total}")),
        "{screen}"
    );
    assert!(screen.contains("tool: shell"), "{screen}");

    app.on_key(key(KeyCode::Down));
    let screen = draw(&mut app, width, 24);
    assert!(
        screen.contains(&format!("lines 2–13 of {total}")),
        "the first visible line moved: {screen}"
    );
    assert!(
        !screen.contains("tool: shell"),
        "the body's first row scrolled away: {screen}"
    );
    // The keys are still the box's last row.
    assert!(screen.contains("allow once"), "{screen}");
}

/// PgUp/PgDn move a page (the visible height), and the window clamps at both ends.
#[test]
fn page_keys_move_a_page_and_clamp() {
    let mut app = shell_app(60);
    draw(&mut app, 80, 24);
    app.on_key(key(KeyCode::PageDown));
    let screen = draw(&mut app, 80, 24);
    let total = render::wrap(
        &render::approval_body(&shell_request(&long_command(60))),
        76,
    )
    .len();
    assert!(
        screen.contains(&format!("lines 13–24 of {total}")),
        "a page is the visible height: {screen}"
    );
    // Far past the end clamps to the last window.
    for _ in 0..10 {
        app.on_key(key(KeyCode::PageDown));
    }
    let screen = draw(&mut app, 80, 24);
    assert!(
        screen.contains(&format!("lines {}–{total} of {total}", total - 11)),
        "the last window is complete: {screen}"
    );
    // And back up one page from the end.
    app.on_key(key(KeyCode::PageUp));
    let screen = draw(&mut app, 80, 24);
    assert!(
        screen.contains(&format!("lines {}–{} of {total}", total - 23, total - 12)),
        "a page back from the end: {screen}"
    );
}

/// A short body shows no window indicator: there is nothing to scroll.
#[test]
fn a_short_body_has_no_window_indicator() {
    let mut app = App::new("mock", "s");
    prompt(&mut app);
    let screen = draw(&mut app, 80, 24);
    assert!(!screen.contains("lines 1–"), "{screen}");
    assert!(screen.contains("allow once"), "{screen}");
}

/// The height is bounded by ~60% of the terminal, so a tall box cannot eat the screen.
#[test]
fn the_box_height_is_capped_at_sixty_percent_of_the_terminal() {
    assert_eq!(render::approval_cap(24, 0), 14);
    assert_eq!(render::approval_cap(60, 0), 36);
    // A short terminal keeps three transcript rows when it can, and never goes below the floor.
    assert_eq!(render::approval_cap(10, 0), 6);
    assert_eq!(render::approval_cap(9, 0), 5);
    // The `@` completion list above the box counts against the transcript's three rows.
    assert_eq!(render::approval_cap(24, 4), 14);
}

/// The shell approval shows the command once: `args` no longer repeats it, but keeps the rest.
#[test]
fn a_shell_approval_shows_the_command_once() {
    let text = render::approval_body(&shell_request("echo once"));
    assert_eq!(
        text.matches("echo once").count(),
        1,
        "the command is displayed twice: {text}"
    );
    assert!(text.contains("cwd"), "the remaining args are kept: {text}");
    // The keys are not body content: a mutation that pushes them back into the wrapped body
    // is how they scrolled out of view in the first place.
    assert!(
        !text.contains("[y] allow once"),
        "the key line must stay out of the body: {text}"
    );
    // And the full text still carries the keys, on their own line.
    let full = render::approval_text(&shell_request("echo once"));
    assert_eq!(
        full.lines().last(),
        Some(render::APPROVAL_KEYS),
        "the key line is the text's own last line: {full}"
    );
}

/// A path tool whose only argument is the path shows no `args` line at all.
#[test]
fn args_are_omitted_when_the_resource_is_the_only_field() {
    let request = ApprovalRequest::new(
        "p",
        "c",
        "write",
        "/ws/a.txt",
        "/ws/a.txt",
        json!({"path": "/ws/a.txt"}),
        None,
    );
    let text = render::approval_body(&request);
    assert!(!text.contains("args"), "{text}");
    assert!(text.contains("path: /ws/a.txt"), "{text}");
}

/// The 400-byte cap applies to the args that are left after the dedup, not to the command.
#[test]
fn the_args_cap_applies_after_the_dedup() {
    let content = "x".repeat(2000);
    let request = ApprovalRequest::new(
        "p",
        "c",
        "write",
        "/ws/a.txt",
        "/ws/a.txt",
        json!({"path": "/ws/a.txt", "content": content}),
        None,
    );
    let text = render::approval_body(&request);
    assert!(text.contains("more bytes"), "{text}");
    assert!(
        text.len() < 1000,
        "the args are still capped: {}",
        text.len()
    );
    // The command itself is never truncated.
    let long = "echo ".to_string() + &"y".repeat(2000);
    let shell = shell_request(&long);
    let text = render::approval_body(&shell);
    assert!(
        text.contains(&"y".repeat(2000)),
        "the command must not be truncated: {}",
        text.len()
    );
}

/// A new pending request starts at the top of its body: the scroll belongs to the modal.
#[test]
fn a_new_pending_request_resets_the_scroll() {
    let mut app = shell_app(60);
    draw(&mut app, 80, 24);
    for _ in 0..3 {
        app.on_key(key(KeyCode::Down));
    }
    let second = PendingView {
        request_id: 10,
        origin: "session".into(),
        request: PendingRequest::Approval(shell_request(&long_command(60))),
    };
    app.update(Input::Pending(Some(Box::new(second)), 1));
    let screen = draw(&mut app, 80, 24);
    let total = render::wrap(
        &render::approval_body(&shell_request(&long_command(60))),
        76,
    )
    .len();
    assert!(
        screen.contains(&format!("lines 1–12 of {total}")),
        "the next question starts at the top: {screen}"
    );
    assert!(screen.contains("tool: shell"), "{screen}");
}

/// The scroll keys do not reach the composer: nothing is typed while scrolling the box.
#[test]
fn scroll_keys_do_not_type_into_the_composer() {
    let mut app = shell_app(60);
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::PageDown));
    assert!(app.input.is_empty(), "{:?}", app.input);
    assert!(
        app.pending_approval().is_some(),
        "scrolling is not an answer"
    );
}
