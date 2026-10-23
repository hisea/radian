//! `/loop`: the continuation engine (steps 1-2, backlog `tui-loop-continuation.md`).
//!
//! Driver-level tests with the injected clock (no sleeps): enabling re-sends the same
//! prompt turn after turn with `source = User` (that half is pinned where the source is
//! attached, `radian-host`'s wake-gate test) and an increasing `iteration` meta; `--times`
//! and `--for` stop with a notice; `--every` waits out the interval; Esc and model errors
//! pause; the footer shows the state and hides when the loop is off; `/help` lists it.

use std::collections::VecDeque;
use std::time::Duration;

use radian_core::{InputSource, Message, MessageId};
use radian_host::config::TuiFooterConfig;
use radian_session::{InboxLane, RuntimeEvent, TurnOutcome};
use radian_tui::app::{
    App, Effect, Entry, EntryKind, Input, TurnState, loop_iteration, loop_message,
};
use radian_tui::mention::Expansion;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Submits a line, running its `@` expansion like the loop, and returns every effect.
fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    let mut seen = Vec::new();
    let mut queue: VecDeque<Effect> = app.on_key(key(KeyCode::Enter)).effects.into();
    while let Some(effect) = queue.pop_front() {
        seen.push(effect.clone());
        if let Effect::ExpandMentions { text } = &effect {
            queue.extend(
                app.update(Input::MentionsExpanded(Ok(Expansion {
                    text: text.clone(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                    warnings: Vec::new(),
                })))
                .effects,
            );
        }
    }
    seen
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

fn enqueued(app: &mut App, id: &str, message: Message) {
    app.update(Input::Runtime(RuntimeEvent::InboxEnqueued {
        message,
        message_id: MessageId(id.to_owned()),
        lane: InboxLane::NextTurn,
        source: InputSource::User,
        wake: true,
        degraded: false,
        pending_wake: false,
    }));
}

fn admitted(app: &mut App, id: &str) {
    app.update(Input::Runtime(RuntimeEvent::InputAdmitted {
        message_id: MessageId(id.to_owned()),
        transcript_index: 0,
    }));
}

fn started(app: &mut App, turn: &str) {
    app.update(Input::Runtime(RuntimeEvent::TurnStarted {
        turn_id: turn.to_owned(),
    }));
}

fn finished(app: &mut App, turn: &str, outcome: TurnOutcome) -> Vec<Effect> {
    app.update(Input::Runtime(RuntimeEvent::TurnFinished {
        turn_id: turn.to_owned(),
        outcome,
        reason: None,
        stop_reason: None,
    }))
    .effects
}

fn failed(app: &mut App, turn: &str, message: &str) {
    app.update(Input::Runtime(RuntimeEvent::TurnFailed {
        turn_id: turn.to_owned(),
        message: message.to_owned(),
    }));
}

/// One full loop iteration in production order: the echo, the start, the admission, then
/// the end. Returns what the turn end produced. `now` is the admission instant (the
/// `--every`/`--for` clocks start there); the end runs at `end_now`.
struct Turn<'a> {
    id: &'a str,
    turn: &'a str,
    prompt: &'a str,
    iteration: u64,
    now: std::time::Instant,
    end_now: std::time::Instant,
    outcome: TurnOutcome,
}

fn run_iteration(app: &mut App, spec: &Turn) -> Vec<Effect> {
    app.set_now(spec.now);
    enqueued(app, spec.id, loop_message(spec.prompt, spec.iteration));
    started(app, spec.turn);
    admitted(app, spec.id);
    app.set_now(spec.end_now);
    finished(app, spec.turn, spec.outcome)
}

fn loop_app() -> (App, std::time::Instant) {
    let mut app = App::new("mock", "s");
    let warnings = app.apply_footer(&TuiFooterConfig {
        preset: Some("deck".to_string()),
        glyphs: Some("unicode".to_string()),
        ..Default::default()
    });
    assert!(warnings.is_empty(), "{warnings:?}");
    let base = app.session.spinner_origin;
    app.set_now(base);
    (app, base)
}

fn screen(app: &mut App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
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

/// Enabling re-sends the same prompt turn after turn, each tagged with the next
/// iteration (`source = User` is attached at the inbox boundary — `radian-host`'s
/// wake-gate test pins that half).
#[test]
fn loop_resends_the_same_prompt_with_increasing_iterations() {
    let (mut app, base) = loop_app();
    let effects = submit(&mut app, "/loop go");
    assert_eq!(
        effects,
        vec![
            Effect::ExpandMentions { text: "go".into() },
            Effect::SendFollowup(loop_message("go", 1)),
        ]
    );
    for (n, turn) in ["t-1", "t-2", "t-3"].iter().enumerate() {
        let iteration = n as u64 + 1;
        let effects = run_iteration(
            &mut app,
            &Turn {
                id: &format!("e-{iteration}"),
                turn,
                prompt: "go",
                iteration,
                now: base,
                end_now: base,
                outcome: TurnOutcome::Completed,
            },
        );
        if iteration < 3 {
            assert_eq!(
                effects,
                vec![Effect::SendFollowup(loop_message("go", iteration + 1))],
                "turn {iteration} re-sends the prompt"
            );
            for effect in &effects {
                if let Effect::SendFollowup(message) = effect {
                    assert_eq!(loop_iteration(message), Some(iteration + 1));
                }
            }
        }
    }
    let rendered = screen(&mut app);
    assert!(
        rendered.contains("loop 3/20"),
        "the footer counts completed iterations:\n{rendered}"
    );
}

/// `--times 2` runs exactly two turns, then stops with a notice and the mode off.
#[test]
fn times_two_stops_after_two_turns_with_a_notice() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop --times 2 go");
    let first = run_iteration(
        &mut app,
        &Turn {
            id: "e-1",
            turn: "t-1",
            prompt: "go",
            iteration: 1,
            now: base,
            end_now: base,
            outcome: TurnOutcome::Completed,
        },
    );
    assert_eq!(first, vec![Effect::SendFollowup(loop_message("go", 2))]);
    let second = run_iteration(
        &mut app,
        &Turn {
            id: "e-2",
            turn: "t-2",
            prompt: "go",
            iteration: 2,
            now: base,
            end_now: base,
            outcome: TurnOutcome::Completed,
        },
    );
    assert!(second.is_empty(), "--times 2 stops after two turns");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "loop finished after 2 turns (--times 2)"),
        "{:?}",
        notices(&app)
    );
    let rendered = screen(&mut app);
    assert!(
        !rendered.contains("loop 2/2"),
        "the footer hides with the loop:\n{rendered}"
    );
}

/// `--every 5m`: a turn that ends after one minute waits; the deadline sends at five
/// minutes; a turn that runs seven minutes sends at its end (never interrupting it).
#[test]
fn every_waits_out_the_interval() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop --every 5m go");
    let minute = base + Duration::from_secs(60);
    let five = base + Duration::from_secs(300);
    let effects = run_iteration(
        &mut app,
        &Turn {
            id: "e-1",
            turn: "t-1",
            prompt: "go",
            iteration: 1,
            now: base,
            end_now: minute,
            outcome: TurnOutcome::Completed,
        },
    );
    assert!(effects.is_empty(), "one minute in: not due");
    let rendered = screen(&mut app);
    assert!(
        rendered.contains("loop · next in 4m"),
        "the footer counts down:\n{rendered}"
    );
    // An early deadline is a no-op (several due ticks merge into one send).
    app.set_now(minute);
    assert!(app.update(Input::LoopDeadline).effects.is_empty());
    app.set_now(five);
    assert_eq!(
        app.update(Input::LoopDeadline).effects,
        vec![Effect::SendFollowup(loop_message("go", 2))]
    );

    // A turn that runs past the interval sends at its end.
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop --every 5m go");
    let seven = base + Duration::from_secs(7 * 60);
    let effects = run_iteration(
        &mut app,
        &Turn {
            id: "e-1",
            turn: "t-1",
            prompt: "go",
            iteration: 1,
            now: base,
            end_now: seven,
            outcome: TurnOutcome::Completed,
        },
    );
    assert_eq!(
        effects,
        vec![Effect::SendFollowup(loop_message("go", 2))],
        "a long turn sends at its end"
    );
}

/// No flags means the default cap: twenty turns, then the 21st is never sent.
#[test]
fn the_default_cap_stops_at_twenty_turns() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop go");
    let mut sends = 1; // the enabling send is iteration 1
    for iteration in 1..=20u64 {
        let effects = run_iteration(
            &mut app,
            &Turn {
                id: &format!("e-{iteration}"),
                turn: &format!("t-{iteration}"),
                prompt: "go",
                iteration,
                now: base,
                end_now: base,
                outcome: TurnOutcome::Completed,
            },
        );
        sends += effects
            .iter()
            .filter(|effect| matches!(effect, Effect::SendFollowup(_)))
            .count();
    }
    assert_eq!(sends, 20, "exactly twenty sends, never a 21st");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "loop finished after 20 turns (the default cap of 20 turns or 1h)"),
        "{:?}",
        notices(&app)
    );
}

/// `--for 1h` stops an hour after the first iteration, with a notice.
#[test]
fn for_duration_stops_with_a_notice() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop --for 1h go");
    let hour = base + Duration::from_secs(3600);
    let effects = run_iteration(
        &mut app,
        &Turn {
            id: "e-1",
            turn: "t-1",
            prompt: "go",
            iteration: 1,
            now: base,
            end_now: hour,
            outcome: TurnOutcome::Completed,
        },
    );
    assert!(effects.is_empty());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "loop finished after 1h (--for 1h)"),
        "{:?}",
        notices(&app)
    );
}

/// Esc on a loop turn pauses (the mode stays on, the prompt is dropped); the next
/// message becomes the new loop prompt and resumes iterating.
#[test]
fn esc_pauses_and_the_next_message_becomes_the_new_loop_prompt() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop go");
    app.set_now(base);
    enqueued(&mut app, "e-1", loop_message("go", 1));
    started(&mut app, "t-1");
    admitted(&mut app, "e-1");
    // The real key: Esc cancels the running turn.
    app.session.projection.set_turn(TurnState::Running);
    let effects = app.on_key(key(KeyCode::Esc)).effects;
    assert_eq!(effects, vec![Effect::Cancel]);
    app.session.projection.set_turn(TurnState::Idle);
    let effects = finished(&mut app, "t-1", TurnOutcome::Cancelled);
    assert!(effects.is_empty(), "a cancelled turn never re-sends");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("loop paused: the turn was cancelled")),
        "{:?}",
        notices(&app)
    );
    let rendered = screen(&mut app);
    assert!(
        rendered.contains("loop paused"),
        "the footer says paused:\n{rendered}"
    );

    let effects = submit(&mut app, "try this instead");
    assert_eq!(
        effects,
        vec![
            Effect::ExpandMentions {
                text: "try this instead".into()
            },
            Effect::SendFollowup(loop_message("try this instead", 1)),
        ]
    );
}

/// A failed turn pauses without re-sending; `/retry` resumes on the same prompt.
#[test]
fn a_failed_turn_pauses_and_retry_resumes() {
    let (mut app, base) = loop_app();
    app.session.entries.push(Entry::new(EntryKind::User, "go"));
    submit(&mut app, "/loop go");
    app.set_now(base);
    enqueued(&mut app, "e-1", loop_message("go", 1));
    started(&mut app, "t-1");
    admitted(&mut app, "e-1");
    failed(&mut app, "t-1", "boom");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("loop paused: the turn failed")),
        "{:?}",
        notices(&app)
    );
    let effects = finished(&mut app, "t-1", TurnOutcome::Failed);
    assert!(effects.is_empty(), "never re-send into a broken provider");

    app.input = "/retry".into();
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert_eq!(effects, vec![Effect::SendFollowup(loop_message("go", 1))]);
}

/// While the loop iterates, a typed message is an ordinary steer — the loop keeps
/// re-sending its fixed prompt ("每次迭代原样重投": only the armed or paused next message
/// becomes the prompt).
#[test]
fn a_typed_message_while_iterating_does_not_replace_the_prompt() {
    let (mut app, base) = loop_app();
    submit(&mut app, "/loop go");
    app.set_now(base);
    enqueued(&mut app, "e-1", loop_message("go", 1));
    started(&mut app, "t-1");
    admitted(&mut app, "e-1");
    app.session.projection.set_turn(TurnState::Running);
    let effects = submit(&mut app, "one-off steer");
    assert_eq!(
        effects,
        vec![
            Effect::ExpandMentions {
                text: "one-off steer".into()
            },
            Effect::Steer(Message::user("one-off steer")),
        ],
        "a steer, not a loop capture"
    );
    app.session.projection.set_turn(TurnState::Idle);
    let effects = finished(&mut app, "t-1", TurnOutcome::Completed);
    assert_eq!(
        effects,
        vec![Effect::SendFollowup(loop_message("go", 2))],
        "the loop keeps its fixed prompt"
    );
}

/// The footer shows the armed, iterating, every and waiting states, and hides with the
/// loop off; `/help` lists `/loop`; a bare toggle arms and disarms.
#[test]
fn footer_help_and_toggle() {
    let (mut app, _) = loop_app();
    assert!(submit(&mut app, "/help").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("/loop")),
        "/help lists /loop: {:?}",
        notices(&app)
    );

    let rendered = screen(&mut app);
    assert!(!rendered.contains("loop 0/20"), "off: no loop item");

    submit(&mut app, "/loop");
    let rendered = screen(&mut app);
    assert!(
        rendered.contains("loop on"),
        "armed: the footer says so:\n{rendered}"
    );

    submit(&mut app, "/loop --every 5m watch it");
    let rendered = screen(&mut app);
    assert!(
        rendered.contains("loop every 5m · 0/20"),
        "iterating with --every:\n{rendered}"
    );

    submit(&mut app, "/loop");
    assert!(
        notices(&app).iter().any(|n| n == "loop off"),
        "{:?}",
        notices(&app)
    );
    let rendered = screen(&mut app);
    assert!(
        !rendered.contains("loop every"),
        "off again: the item hides:\n{rendered}"
    );
}
