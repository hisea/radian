//! `/queue <text>`: a followup even while a turn runs, with a visible queue count.
//!
//! Pure `update` tests (ADR 0023 U3) plus a `TestBackend` render of the footer's `queued`
//! item: submitting `/queue` expands `@` references like a send and always becomes
//! `Effect::SendFollowup` (never `Steer`), and the footer shows the `next_turn` inputs the
//! runtime has not admitted yet (2, then 1, then nothing).

use std::collections::VecDeque;

use agent_core::{InputSource, Message, MessageId};
use radian_host::config::TuiFooterConfig;
use radian_session::{InboxLane, RuntimeEvent};
use radian_tui::app::{App, Effect, Input, TurnState};
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
        .filter(|entry| entry.kind == radian_tui::app::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// `/queue` is a followup when idle **and** while a turn runs: Enter steers then, `/queue`
/// never does. Sending a `steer` instead of a `followup` while running fails this test.
#[test]
fn queue_sends_a_followup_even_while_a_turn_runs() {
    let mut app = App::new("m", "s");
    assert_eq!(
        submit(&mut app, "/queue do this next"),
        vec![
            Effect::ExpandMentions {
                text: "do this next".into()
            },
            Effect::SendFollowup(Message::user("do this next"))
        ]
    );

    // While a turn runs an ordinary Enter steers; `/queue` still follows up.
    app.session.projection.set_turn(TurnState::Running);
    assert_eq!(
        submit(&mut app, "/queue and then this"),
        vec![
            Effect::ExpandMentions {
                text: "and then this".into()
            },
            Effect::SendFollowup(Message::user("and then this"))
        ]
    );
}

/// `/queue` with no text is a usage notice; `//queue …` is still the escape (an ordinary
/// message starting with one slash).
#[test]
fn queue_without_text_is_a_usage_notice_and_the_escape_still_sends() {
    let mut app = App::new("m", "s");
    assert!(submit(&mut app, "/queue").is_empty());
    assert!(submit(&mut app, "/queue   ").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("usage: /queue")),
        "{:?}",
        notices(&app)
    );

    assert_eq!(
        submit(&mut app, "//queue x"),
        vec![
            Effect::ExpandMentions {
                text: "/queue x".into()
            },
            Effect::SendFollowup(Message::user("/queue x"))
        ]
    );
}

/// Two rapid submits must not drop one: the second submit waits behind the first one's
/// `@` expansion instead of overwriting it (the pty test caught this: `/queue beta` never
/// reached the runtime when both lines arrived together — the single pending slot kept only
/// the last submit, and the orphaned result sent nothing).
#[test]
fn two_rapid_submits_both_send_in_order() {
    let mut app = App::new("m", "s");
    // Both lines submitted before either expansion returns, the way two pty lines arrive.
    app.input = "/queue alpha".into();
    assert_eq!(
        app.on_key(key(KeyCode::Enter)).effects,
        vec![Effect::ExpandMentions {
            text: "alpha".into()
        }]
    );
    app.input = "/queue beta".into();
    assert_eq!(
        app.on_key(key(KeyCode::Enter)).effects,
        vec![Effect::ExpandMentions {
            text: "beta".into()
        }]
    );
    // The results come back in effect order; each sends its own text.
    let expand = |text: &str| {
        Input::MentionsExpanded(Ok(Expansion {
            text: text.to_owned(),
            attachments: Vec::new(),
            warnings: Vec::new(),
        }))
    };
    assert_eq!(
        app.update(expand("alpha")).effects,
        vec![Effect::SendFollowup(Message::user("alpha"))]
    );
    assert_eq!(
        app.update(expand("beta")).effects,
        vec![Effect::SendFollowup(Message::user("beta"))]
    );
}

fn enqueued(id: &str, lane: InboxLane) -> Input {
    Input::Runtime(RuntimeEvent::InboxEnqueued {
        message: Message::user("queued text"),
        message_id: MessageId(id.to_owned()),
        lane,
        source: InputSource::User,
        wake: true,
        degraded: false,
        pending_wake: false,
    })
}

fn admitted(id: &str) -> Input {
    Input::Runtime(RuntimeEvent::InputAdmitted {
        message_id: MessageId(id.to_owned()),
        transcript_index: 0,
    })
}

fn queued_app() -> App {
    let mut app = App::new("mock", "s");
    let warnings = app.apply_footer(&TuiFooterConfig {
        preset: Some("deck".to_string()),
        glyphs: Some("unicode".to_string()),
        ..Default::default()
    });
    assert!(warnings.is_empty(), "{warnings:?}");
    app
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

/// The `queued` footer count only moves on a followup's own enqueue and admission: a
/// `NextStep` enqueue never enters it, a duplicate enqueue cannot double-count, an
/// admission for an unknown id cannot move it, and draining stops at zero. In particular
/// the count never decreases except by admitting what was queued.
#[test]
fn the_queued_footer_count_tracks_only_unadmitted_followups() {
    let mut app = queued_app();

    // A steer/injection enqueue is `NextStep`: it never enters the count.
    app.update(enqueued("s-1", InboxLane::NextStep));
    assert!(
        !screen(&mut app).contains("queued"),
        "a steer must not show:\n{}",
        screen(&mut app)
    );

    app.update(enqueued("q-1", InboxLane::NextTurn));
    app.update(enqueued("q-2", InboxLane::NextTurn));
    let two = screen(&mut app);
    assert!(two.contains("2 queued"), "two waiting:\n{two}");

    // A re-enqueue of the same id cannot double-count.
    app.update(enqueued("q-1", InboxLane::NextTurn));
    let still_two = screen(&mut app);
    assert!(
        still_two.contains("2 queued"),
        "no double count:\n{still_two}"
    );

    // An admission for an id that was never queued cannot move the count — neither an
    // unknown id nor a steer's.
    app.update(admitted("nope"));
    app.update(admitted("s-1"));
    let still_two = screen(&mut app);
    assert!(
        still_two.contains("2 queued"),
        "unknown/steer admissions must not move it:\n{still_two}"
    );

    // Admissions drain one at a time; at zero the item hides.
    app.update(admitted("q-1"));
    let one = screen(&mut app);
    assert!(one.contains("1 queued"), "one left:\n{one}");
    assert!(
        !one.contains("2 queued"),
        "it decreased by exactly one:\n{one}"
    );

    app.update(admitted("q-2"));
    let none = screen(&mut app);
    assert!(!none.contains("queued"), "drained: the item hides:\n{none}");

    // Draining past zero is a no-op, never negative.
    app.update(admitted("q-2"));
    assert!(
        !screen(&mut app).contains("queued"),
        "no negative count:\n{}",
        screen(&mut app)
    );
}
