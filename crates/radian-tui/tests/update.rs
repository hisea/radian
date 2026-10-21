//! The pure `App::update` (ADR 0023 U3): every effect's trigger, and what each result `Input`
//! does to the state. No terminal, no runtime, no files.

use agent_core::CompactionOutcome;
use radian_tui::app::{App, Effect, Input, MemoryResult, TurnState};
use radian_tui::command::{ExportRequest, SessionOutcome};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

/// Submits one line and returns the effects the key produced (the loop would run them).
fn submit(app: &mut App, line: &str) -> Vec<Effect> {
    app.input = line.into();
    app.on_key(key(KeyCode::Enter)).effects
}

fn notices(app: &App) -> Vec<String> {
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == radian_tui::EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// Every `/…` command that needs the outside world asks for it with one effect, and never
/// sends anything itself.
#[test]
fn every_slash_command_produces_its_effect() {
    let mut app = App::new("m", "s");
    app.set_memory_enabled(true);

    assert_eq!(
        submit(&mut app, "/compact"),
        vec![Effect::Compact { instructions: None }]
    );
    assert_eq!(
        submit(&mut app, "/compact focus on the parser"),
        vec![Effect::Compact {
            instructions: Some("focus on the parser".into())
        }]
    );
    assert_eq!(
        submit(&mut app, "/sessions"),
        vec![Effect::OpenPicker { query: None }]
    );
    assert_eq!(
        submit(&mut app, "/sessions parser bug"),
        vec![Effect::OpenPicker {
            query: Some("parser bug".into())
        }]
    );
    assert_eq!(
        submit(&mut app, "/export json out.json --force"),
        vec![Effect::Session(SessionOutcome::Export(ExportRequest {
            format: radian_host::Format::Json,
            path: Some("out.json".into()),
            force: true,
        }))]
    );
    assert_eq!(
        submit(&mut app, "/fork"),
        vec![Effect::Session(SessionOutcome::Fork)]
    );
    let memory = submit(&mut app, "/memory list");
    assert!(
        matches!(
            memory.as_slice(),
            [Effect::RunMemoryCommand {
                confirmed: false,
                ..
            }]
        ),
        "{memory:?}"
    );
}

/// ADR 0022 §1: `/fork` needs an idle session. The refusal is a decision on app state, so it
/// is made in `update` (a notice, no effect).
#[test]
fn fork_is_refused_while_a_turn_runs() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    let effects = submit(&mut app, "/fork");
    assert!(effects.is_empty(), "no fork while running: {effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("finish the current turn before forking")),
        "{:?}",
        notices(&app)
    );
}

/// `/new` ends the session when idle: no effect (leaving the loop is the execution), and
/// the flags the loop reads to end this session and open the next.
#[test]
fn new_starts_a_new_session_when_idle() {
    let mut app = App::new("m", "s");
    let effects = submit(&mut app, "/new");
    assert!(effects.is_empty(), "leaving needs no effect: {effects:?}");
    assert!(app.new_session, "the loop must open a new session");
    assert!(app.quit, "the loop must leave this session");
    assert!(notices(&app).is_empty(), "{:?}", notices(&app));
}

/// `/new` while a turn runs is refused with a notice and the session is unchanged.
#[test]
fn new_is_refused_while_a_turn_runs() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    let effects = submit(&mut app, "/new");
    assert!(
        effects.is_empty(),
        "no new session while running: {effects:?}"
    );
    assert!(!app.new_session && !app.quit, "the session is unchanged");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("finish the current turn before starting a new session")),
        "{:?}",
        notices(&app)
    );
}

/// `/new` while a background job runs is refused with a notice pointing to `/jobs kill`,
/// and the session is unchanged.
#[test]
fn new_is_refused_while_a_background_job_runs() {
    let mut app = App::new("m", "s");
    app.update(Input::Runtime(
        radian_session::RuntimeEvent::BackgroundJobStarted {
            job_id: "job-1".to_owned(),
            command: "sleep 60".to_owned(),
            output: "available through job".to_owned(),
        },
    ));
    let effects = submit(&mut app, "/new");
    assert!(effects.is_empty(), "no new session with a job: {effects:?}");
    assert!(!app.new_session && !app.quit, "the session is unchanged");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("`/jobs kill`")),
        "{:?}",
        notices(&app)
    );
}

/// `/new` after the session ended is refused (like `/fork`), and the session is unchanged.
#[test]
fn new_is_refused_after_the_session_ended() {
    let mut app = App::new("m", "s");
    app.session.session_ended = Some("the driver failed".to_owned());
    let effects = submit(&mut app, "/new");
    assert!(effects.is_empty(), "no new session when ended: {effects:?}");
    assert!(!app.new_session && !app.quit, "the session is unchanged");
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("needs a live session")),
        "{:?}",
        notices(&app)
    );
}

/// The result inputs: each one changes the state the way the loop's result used to.
#[test]
fn the_result_inputs_change_the_state() {
    let mut app = App::new("m", "s");

    // A manual `/compact` outcome: compacted, nothing to do, and a failure.
    app.update(Input::CompactDone(Ok(CompactionOutcome::Compacted {
        revision: 1,
        tokens_before: 100,
        tokens_after: 40,
    })));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction: 100 → 40 tokens"),
        "{:?}",
        notices(&app)
    );
    app.update(Input::CompactDone(Ok(CompactionOutcome::NothingToCompact)));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction: nothing to compact")
    );
    app.update(Input::CompactDone(Err("boom".into())));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == "compaction failed: boom")
    );

    // `/sessions`, `/export` and `/fork` results are lines.
    app.update(Input::SessionDone(vec!["one".into(), "two".into()]));
    assert!(
        notices(&app).iter().any(|note| note == "one")
            && notices(&app).iter().any(|note| note == "two")
    );

    // A `/memory` output, and a failure (including "memory is disabled").
    app.update(Input::MemoryCommandDone(MemoryResult::Output(vec![
        "out".into(),
    ])));
    assert!(notices(&app).iter().any(|note| note == "out"));
    app.update(Input::MemoryCommandDone(MemoryResult::Failed(
        "memory is disabled (no RADIAN_HOME or HOME)".into(),
    )));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("memory is disabled"))
    );
}

/// A runtime turn failure is a transcript notice; a recognizable context overflow adds the
/// one-line help (ADR 0018's fallback).
#[test]
fn a_failed_turn_is_a_notice_and_an_overflow_adds_the_help() {
    let mut app = App::new("m", "s");
    app.update(Input::Runtime(radian_session::RuntimeEvent::TurnFailed {
        turn_id: "t-1".into(),
        message: "boom".into(),
    }));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note.contains("turn failed: boom") && note.contains("/retry to resend")),
        "{:?}",
        notices(&app)
    );

    let mut app = App::new("m", "s");
    app.update(Input::Runtime(radian_session::RuntimeEvent::TurnFailed {
        turn_id: "t-1".into(),
        message: format!("{} HTTP 400", radian_host::CONTEXT_OVERFLOW_PREFIX),
    }));
    assert!(
        notices(&app)
            .iter()
            .any(|note| note == radian_host::CONTEXT_OVERFLOW_HELP),
        "{:?}",
        notices(&app)
    );
}

/// Paste, resize and the `/memory` confirmation answer.
#[test]
fn paste_resize_and_the_confirmation_answer() {
    let mut app = App::new("m", "s");
    app.update(Input::Paste("one\ntwo".into()));
    assert_eq!(app.input, "one\ntwo");
    assert!(
        app.update(Input::Resize {
            rows: 40,
            cols: 100
        })
        .dirty
    );

    // `y`/`n` without a pending command do nothing at all.
    assert!(app.update(Input::ConfirmCommand(true)).effects.is_empty());
    assert!(app.update(Input::ConfirmCommand(false)).effects.is_empty());
}

/// `dirty` is false when nothing the renderer reads changed, so the loop can skip the frame
/// (ADR 0023 U2's zero-redraw rule rests on this).
#[test]
fn an_input_that_changes_nothing_is_not_dirty() {
    let mut app = App::new("m", "s");
    // An unbound key is not an input at all.
    let none = app.on_key(key(KeyCode::F(5)));
    assert!(!none.dirty && none.effects.is_empty());
    // Cancel and quit leave the frame alone (the turn state changes when the turn ends).
    app.session.projection.set_turn(TurnState::Running);
    let cancel = app.on_key(ctrl(KeyCode::Char('c')));
    assert_eq!(cancel.effects, vec![Effect::Cancel]);
    assert!(!cancel.dirty, "cancelling does not repaint");
    app.session.projection.set_turn(TurnState::Idle);
    // An idle Ctrl-C now arms the 2 s quit window: that *does* repaint
    // (the hint appears), and the second press quits without repainting.
    let arm = app.on_key(ctrl(KeyCode::Char('c')));
    assert!(arm.effects.is_empty(), "the first press only arms");
    assert!(arm.dirty, "the hint must be drawn");
    let quit = app.on_key(ctrl(KeyCode::Char('c')));
    assert_eq!(quit.effects, vec![Effect::Quit]);
    assert!(!quit.dirty, "quitting does not repaint");
    // Typing does.
    assert!(app.on_key(key(KeyCode::Char('a'))).dirty);
}

/// ADR 0023 U5: a command the session's facade refuses (an undeclared capability) becomes a
/// notice, not a failed loop. The UI declares what it can do from `capabilities()`, but the
/// facade is the authority.
#[test]
fn a_refused_command_is_shown_as_a_notice() {
    let mut app = App::new("m", "s");
    let refused = radian_host::CommandRefused::undeclared(
        &radian_host::SessionCommand::SendFollowup(agent_core::Message::user("hi")),
    );
    let result = app.update(Input::CommandRefused(refused));
    assert!(result.dirty, "a notice is a visible change");
    let shown = notices(&app);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].contains("refused `send`") && shown[0].contains("does not declare"),
        "{shown:?}"
    );
}

/// The `/…` completion walks the command tree: typing shows the candidates, Tab completes the
/// name (never executing it), the next level's list appears, and Tab completes that too.
#[test]
fn the_slash_completion_walks_the_command_tree() {
    let mut app = App::new("m", "s");
    for c in "/mem".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let rows = app.completion().expect("a list").to_vec();
    assert!(
        rows.iter().any(|row| row.insert.starts_with("/memory")),
        "{rows:?}"
    );

    // Tab completes the name and does not run anything.
    let result = app.on_key(key(KeyCode::Tab));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.input, "/memory ");
    // The next level's list appears without another key.
    let rows = app.completion().expect("the subcommand list").to_vec();
    assert!(
        rows.iter().any(|row| row.insert.starts_with("list")),
        "{rows:?}"
    );

    // A partial second level is fuzzy-matched, and Tab completes it.
    app.on_key(key(KeyCode::Char('f')));
    app.on_key(key(KeyCode::Char('o')));
    let rows = app.completion().expect("the fuzzy list").to_vec();
    assert!(
        rows.iter().any(|row| row.insert.starts_with("forget")),
        "{rows:?}"
    );
    let result = app.on_key(key(KeyCode::Tab));
    assert_eq!(app.input, "/memory forget ");
    assert!(
        matches!(result.effects.as_slice(), [Effect::FetchCompletions(_)]),
        "the id level needs the host: {:?}",
        result.effects
    );

    // Enter picks the candidate too (it never executes), and Esc closes the list without
    // clearing the text.
    let mut app = App::new("m", "s");
    for c in "/memory f".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(app.completion().is_some(), "the list follows the text");
    let result = app.on_key(key(KeyCode::Enter));
    assert!(
        !matches!(result.effects.as_slice(), [Effect::SendFollowup(_)]),
        "Enter completed, it did not send: {:?}",
        result.effects
    );
    assert_eq!(app.input, "/memory forget ");

    let mut app = App::new("m", "s");
    for c in "/memory ".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(app.completion().is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.completion().is_none(), "Esc closes the list");
    assert_eq!(app.input, "/memory ", "and keeps the text");
}

/// Owner bug report 2026-10-17 (`pty_model_switches_to_chatgpt_with_an_oauth_login` flaked
/// under load): the `/model` answer for an **exact** route must never arm the menu, even when a
/// longer model id shares its prefix (`gpt-5.4` vs. `gpt-5.4-mini`, `radian-host`'s descriptor).
/// Armed, Enter accepts the highlighted row instead of submitting the finished line — the switch
/// is never queued and, because the answer's arrival races the Enter key, the same input passes
/// or fails depending on load. The rule is `command::is_exact_candidate`, asked by both
/// completion paths.
#[test]
fn an_exact_model_route_is_not_armed_even_with_a_prefix_sibling() {
    let mut app = App::new("m", "s");
    for c in "/model chatgpt/gpt-5.4".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let result = app.update(Input::CompletionsReady {
        fetch: radian_tui::command::Fetch::ModelRoute {
            provider: None,
            partial: "chatgpt/gpt-5.4".into(),
        },
        candidates: vec![
            candidate("chatgpt/gpt-5.4", "chatgpt"),
            candidate("chatgpt/gpt-5.4-mini", "chatgpt"),
        ],
    });
    assert!(result.dirty);
    assert!(
        app.completion().is_none(),
        "an exact route is not a completion to accept: {:?}",
        app.completion()
    );
    // Enter submits the finished line: the switch, never a candidate acceptance.
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ModelSwitch { provider, model }]
                if provider == "chatgpt" && model == "gpt-5.4"
        ),
        "Enter must submit `/model chatgpt/gpt-5.4`: {effects:?}"
    );
    assert!(app.input.is_empty(), "the submitted line is taken");
}

/// The sibling of the test above: a **partial** word is still completed, prefix siblings and all,
/// so the fix above cannot be "never offer a model whose id starts with the typed word".
#[test]
fn a_partial_model_route_still_lists_its_prefix_siblings() {
    let mut app = App::new("m", "s");
    for c in "/model chatgpt/gpt-5".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.update(Input::CompletionsReady {
        fetch: radian_tui::command::Fetch::ModelRoute {
            provider: None,
            partial: "chatgpt/gpt-5".into(),
        },
        candidates: vec![
            candidate("chatgpt/gpt-5.4", "chatgpt"),
            candidate("chatgpt/gpt-5.4-mini", "chatgpt"),
        ],
    });
    let rows = app.completion().expect("the partial lists both").to_vec();
    assert_eq!(rows.len(), 2, "{rows:?}");
    // Enter accepts the highlighted row (it never runs the command).
    let effects = app.on_key(key(KeyCode::Enter)).effects;
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ModelSwitch { .. })),
        "a completion was picked, not sent: {effects:?}"
    );
    assert_eq!(app.input, "/model chatgpt/gpt-5.4 ");
}

fn candidate(insert: &str, description: &str) -> radian_tui::command::Candidate {
    radian_tui::command::Candidate {
        insert: insert.into(),
        description: description.into(),
        current: false,
    }
}

/// A level whose candidates need the host asks with an effect; the answer becomes the list, and
/// a stale answer (for a fetch the line has moved past) is dropped.
#[test]
fn the_id_level_asks_the_host_and_shows_the_answer() {
    let mut app = App::new("m", "s");
    let mut effects = Vec::new();
    for c in "/memory forget ".chars() {
        effects = app.on_key(key(KeyCode::Char(c))).effects;
    }
    let fetch = match effects.as_slice() {
        [Effect::FetchCompletions(fetch)] => fetch.clone(),
        other => panic!("expected one fetch, got {other:?}"),
    };
    assert!(app.completion().is_none(), "no list until the host answers");

    let result = app.update(Input::CompletionsReady {
        fetch: fetch.clone(),
        candidates: vec![radian_tui::command::Candidate {
            insert: "m-1".into(),
            description: "fact note".into(),
            current: false,
        }],
    });
    assert!(result.dirty);
    let rows = app.completion().expect("the id list").to_vec();
    assert!(
        rows.iter().any(|row| row.insert.starts_with("m-1")),
        "{rows:?}"
    );
    let result = app.on_key(key(KeyCode::Tab));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(app.input, "/memory forget m-1 ");

    // A stale answer is dropped: ask for `ab`, then answer with a *different* fetch.
    let mut app = App::new("m", "s");
    for c in "/memory show ab".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let result = app.update(Input::CompletionsReady {
        fetch: radian_tui::command::Fetch::MemoryRecords {
            command: "forget".into(),
            partial: "ab".into(),
        },
        candidates: vec![radian_tui::command::Candidate {
            insert: "stale".into(),
            description: String::new(),
            current: false,
        }],
    });
    assert!(!result.dirty);
    assert!(app.completion().is_none(), "a stale answer shows nothing");
}

/// The slash menu highlights its first row; Up/Down move the highlight (and wrap), and Enter
/// accepts the highlighted row rather than the first. The command-name level offers one best
/// completion, so the menu proper is the argument level.
#[test]
fn the_slash_menu_moves_its_highlight_and_enter_accepts_it() {
    let mut app = App::new("m", "s");
    for c in "/memory ".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let rows = app.completion().expect("a list").to_vec();
    assert!(rows.len() >= 2, "need at least two rows: {rows:?}");
    assert_eq!(app.completion_cursor, 0, "the first row is highlighted");
    let second = rows[1].insert.clone();

    app.on_key(key(KeyCode::Down));
    assert_eq!(app.completion_cursor, 1);
    let result = app.on_key(key(KeyCode::Enter));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(
        app.input,
        format!("/memory {second} ").as_str(),
        "Enter accepts the highlighted row"
    );

    // Up from the first row wraps to the last.
    let mut app = App::new("m", "s");
    for c in "/memory ".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let last = app.completion().unwrap().last().unwrap().insert.clone();
    app.on_key(key(KeyCode::Up));
    let result = app.on_key(key(KeyCode::Tab));
    assert!(result.effects.is_empty(), "{:?}", result.effects);
    assert_eq!(
        app.input,
        format!("/memory {last} ").as_str(),
        "up wraps to the last row"
    );
}

/// Typing after the menu filters it and puts the highlight back on the first row.
#[test]
fn typing_filters_the_slash_menu_and_resets_the_highlight() {
    let mut app = App::new("m", "s");
    for c in "/memory ".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(app.completion().is_some());
    app.on_key(key(KeyCode::Down));
    assert_eq!(app.completion_cursor, 1);
    // `f` narrows the level; the highlight is back on the first row.
    app.on_key(key(KeyCode::Char('f')));
    assert_eq!(app.completion_cursor, 0);
    let rows = app.completion().expect("the filtered list").to_vec();
    assert!(
        rows.iter().any(|row| row.insert.starts_with("forget")),
        "the list follows the new text: {rows:?}"
    );
}

/// ADR 0031 T3: the startup trust modal's **Esc** is the fail-safe answer —
/// it must never trust. It maps to `UntrustSession`: do not trust **this session only**, which
/// is **not persistent**, so the loop writes no `trust.json` and the next start asks again.
#[test]
fn escaping_the_trust_modal_untrusts_this_session_only() {
    let mut app = App::new("m", "s");
    app.push_trust_modal(std::path::PathBuf::from("/work/example"), None, None);
    assert!(app.has_modal());
    let effects = app.on_key(key(KeyCode::Esc)).effects;
    assert_eq!(
        effects,
        vec![Effect::SetTrust {
            choice: radian_host::trust::Choice::UntrustSession
        }],
        "Esc must choose `do not trust, this session only`, never a trust"
    );
    assert!(!app.has_modal(), "the modal is dismissed");
    assert!(
        !radian_host::trust::Choice::UntrustSession.persistent(),
        "Esc writes no trust.json: the next start asks again"
    );

    // Enter takes the highlighted default: the sandbox choice (trust this directory and
    // remember `workspace` for the project). It is persistent, unlike Esc.
    let mut app = App::new("m", "s");
    app.push_trust_modal(std::path::PathBuf::from("/work/example"), None, None);
    assert_eq!(
        app.on_key(key(KeyCode::Enter)).effects,
        vec![Effect::SetTrust {
            choice: radian_host::trust::Choice::TrustHereSandbox
        }]
    );
    assert!(radian_host::trust::Choice::TrustHereSandbox.persistent());
}

/// Esc with an empty input and a running turn cancels it (README): the app-level effect.
#[test]
fn esc_with_an_empty_input_and_a_running_turn_cancels() {
    let mut app = App::new("m", "s");
    app.session.projection.set_turn(TurnState::Running);
    let result = app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        matches!(result.effects.as_slice(), [Effect::Cancel]),
        "{:?}",
        result.effects
    );
}

/// The driver stopped: the session is over, so a submit cannot start a turn. Every submit
/// says so once (with the way back in) and sends nothing — the input is kept, never accepted
/// into an inbox no driver will read.
#[test]
fn a_submit_after_the_session_ended_says_so_and_sends_nothing() {
    let mut app = App::new("m", "sess-7");
    app.session.session_ended = Some("driver failed: ENOSPC".to_owned());

    let first = submit(&mut app, "hello there");
    assert!(
        first.is_empty(),
        "nothing is sent after the session ended: {first:?}"
    );
    assert_eq!(app.input, "hello there", "the message is kept, not lost");
    let first_notices = notices(&app);
    assert_eq!(first_notices.len(), 1, "{first_notices:?}");
    assert!(
        first_notices[0].contains("the session has ended")
            && first_notices[0].contains("driver failed: ENOSPC")
            && first_notices[0].contains("the message was not sent")
            && first_notices[0].contains("radian --resume sess-7"),
        "{}",
        first_notices[0]
    );

    // A second submit says why again: the refusal is never silent.
    let second = submit(&mut app, "another one");
    assert!(second.is_empty(), "{second:?}");
    assert_eq!(notices(&app).len(), 2, "one notice per submit");
    assert_eq!(app.input, "another one");
}

/// The gate is on messages, not on the app-only commands: `/help` still answers after the
/// driver stopped (it never needed the session).
#[test]
fn an_app_only_command_still_runs_after_the_session_ended() {
    let mut app = App::new("m", "sess-8");
    app.session.session_ended = Some("driver failed: ENOSPC".to_owned());
    let effects = submit(&mut app, "/help");
    assert!(effects.is_empty(), "{effects:?}");
    assert!(
        notices(&app).iter().any(|note| note.contains("/compact")),
        "{:?}",
        notices(&app)
    );
}

/// ADR 0008 §2: with no model routed, a submit sends nothing and keeps the draft, pointing at
/// `/model` (and `/login`).
#[test]
fn a_submit_with_no_model_sends_nothing_and_keeps_the_draft() {
    let mut app = App::new("", "sess-nm");
    app.set_has_model(false);
    let effects = submit(&mut app, "hello there");
    assert!(effects.is_empty(), "no model call may start: {effects:?}");
    assert_eq!(app.input, "hello there", "the message is kept, not lost");
    let all = notices(&app);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(
        all[0].contains("no model is set") && all[0].contains("/model"),
        "{}",
        all[0]
    );
}

/// ADR 0008 §2: the first `/model` queue unblocks send and records the route for the footer.
#[test]
fn the_first_model_queue_unblocks_send() {
    let mut app = App::new("", "sess-nm");
    app.set_has_model(false);
    let effects = submit(&mut app, "/model anthropic/m-1");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ModelSwitch { provider, model }]
            if provider == "anthropic" && model == "m-1"
        ),
        "{effects:?}"
    );
    // The loop vets the target, then the app queues it.
    let result = app.update(Input::ModelChecked {
        provider: "anthropic".to_owned(),
        model: "m-1".to_owned(),
        tokens: None,
        budget: None,
    });
    assert!(
        result.effects.iter().any(|effect| matches!(
            effect,
            Effect::QueueModel { provider, model }
            if provider == "anthropic" && model == "m-1"
        )),
        "{:?}",
        result.effects
    );
    assert!(
        app.session.has_model,
        "send is unblocked after the first queue"
    );
    assert_eq!(app.session.model_name, "m-1");
    assert_eq!(app.session.provider.as_deref(), Some("anthropic"));
    // A message now expands (the loop's mention step), not refused.
    let effects = submit(&mut app, "hello");
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExpandMentions { .. })),
        "a message sends once a model is queued: {effects:?}"
    );
}

/// ADR 0008 §2: `/compact` needs a model to summarize with.
#[test]
fn compact_with_no_model_says_so_and_compacts_nothing() {
    let mut app = App::new("", "sess-nm");
    app.set_has_model(false);
    let effects = submit(&mut app, "/compact");
    assert!(effects.is_empty(), "{effects:?}");
    let all = notices(&app);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(all[0].contains("no model is set"), "{}", all[0]);
}
