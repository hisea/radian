//! The slash-command layer in `App::update`: `/help`, `/memory`, the `//` escape, confirmations
//! and unknown commands (ADR 0021 D7). No process is started.
//!
//! The app is pure (ADR 0023 U3), so a `/memory` command becomes an effect; these tests run it
//! the way the event loop does and feed the result back in.

use std::collections::VecDeque;
use std::path::PathBuf;

use radian_host::memory::{MemoryService, ScopeSelection, WriteScope};
use radian_memory::Kind;
use radian_tui::app::{App, Effect, EntryKind, Input, MemoryResult, TurnState};
use radian_tui::mention::Expansion;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-tui-command-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// An `App` wired to a fresh project memory service.
fn app_with_memory(label: &str) -> (PathBuf, App, MemoryService) {
    let dir = scratch(label);
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let service = MemoryService::open(&ws, dir.join("memory"));
    service.create_project().unwrap();
    let mut app = App::new("m", "s");
    app.set_memory_enabled(true);
    (dir, app, service)
}

fn notices(app: &App) -> Vec<String> {
    app.entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// Runs one effect the way the event loop does, for the arms these tests need, and returns the
/// inputs its result produces.
fn run_effect(effect: &Effect, service: &MemoryService) -> Vec<Input> {
    match effect {
        Effect::RunMemoryCommand { command, confirmed } => {
            let json = command.json();
            let result = match radian_host::memory::commands::execute(
                service,
                command.clone(),
                *confirmed,
                json,
            ) {
                Ok(output) => match output.needs_confirmation {
                    Some(prompt) => MemoryResult::NeedsConfirmation {
                        command: command.clone(),
                        prompt,
                    },
                    None => MemoryResult::Output(radian_tui::command::output_notices(&output)),
                },
                Err(error) => MemoryResult::Failed(error.message),
            };
            vec![Input::MemoryCommandDone(result)]
        }
        // A message with no `@` reference: the expansion is the identity.
        Effect::ExpandMentions { text } => vec![Input::MentionsExpanded(Ok(Expansion {
            text: text.clone(),
            attachments: Vec::new(),
            warnings: Vec::new(),
        }))],
        _ => Vec::new(),
    }
}

/// Submits a line, runs its effects like the loop, and returns **every** effect it produced
/// (the key's own, plus the ones the results produced).
fn submit(app: &mut App, service: &MemoryService, line: &str) -> Vec<Effect> {
    app.input = line.into();
    let mut seen = Vec::new();
    let mut queue: VecDeque<Effect> = app.on_key(key(KeyCode::Enter)).effects.into();
    while let Some(effect) = queue.pop_front() {
        seen.push(effect.clone());
        for input in run_effect(&effect, service) {
            queue.extend(app.update(input).effects);
        }
    }
    seen
}

#[test]
fn help_and_unbalanced_quotes_are_notices_and_send_nothing() {
    let (dir, mut app, service) = app_with_memory("help");
    assert!(submit(&mut app, &service, "/help").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("/memory")),
        "{:?}",
        notices(&app)
    );

    assert!(submit(&mut app, &service, "/unknown").is_empty());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("unknown command /unknown")),
        "{:?}",
        notices(&app)
    );

    assert!(submit(&mut app, &service, "/memory add \"unbalanced").is_empty());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("unbalanced quotes")),
        "{:?}",
        notices(&app)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_double_slash_sends_the_message_with_one_slash() {
    let (dir, mut app, service) = app_with_memory("escape");
    // Two steps: the `@` expansion, then the send it turns into.
    assert_eq!(
        submit(&mut app, &service, "//hi"),
        vec![
            Effect::ExpandMentions { text: "/hi".into() },
            Effect::SendFollowup(agent_core::Message::user("/hi"))
        ]
    );
    app.projection.set_turn(TurnState::Running);
    assert_eq!(
        submit(&mut app, &service, "//steer me"),
        vec![
            Effect::ExpandMentions {
                text: "/steer me".into()
            },
            Effect::Steer(agent_core::Message::user("/steer me"))
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_memory_read_runs_and_is_never_sent() {
    let (dir, mut app, service) = app_with_memory("list");
    service
        .add(WriteScope::Project, Kind::Fact, None, "hello world".into())
        .unwrap();
    let effects = submit(&mut app, &service, "/memory list");
    assert_eq!(effects.len(), 1, "{effects:?}");
    match &effects[0] {
        Effect::RunMemoryCommand { command, confirmed } => {
            assert!(!confirmed);
            assert!(format!("{command:?}").contains("List"), "{command:?}");
        }
        other => panic!("expected a memory command, got {other:?}"),
    }
    assert!(
        notices(&app).iter().any(|n| n.contains("hello world")),
        "{:?}",
        notices(&app)
    );

    // While a turn runs a read command still runs and does not steer the turn.
    app.projection.set_turn(TurnState::Running);
    let effects = submit(&mut app, &service, "/memory list");
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::RunMemoryCommand {
                confirmed: false,
                ..
            }]
        ),
        "{effects:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_confirmed_forget_waits_for_y_and_n_keeps_the_record() {
    let (dir, mut app, service) = app_with_memory("forget");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "keep me".into())
        .unwrap();

    let effects = submit(&mut app, &service, &format!("/memory forget {}", record.id));
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::RunMemoryCommand {
                confirmed: false,
                ..
            }]
        ),
        "{effects:?}"
    );
    assert!(app.pending_command().is_some(), "the command waits for y/n");
    assert!(
        notices(&app).iter().any(|n| n.contains("[y/N]")),
        "{:?}",
        notices(&app)
    );

    // `n` cancels; the record is untouched and nothing is run.
    let cancel = app.on_key(key(KeyCode::Char('n')));
    assert!(cancel.effects.is_empty(), "{:?}", cancel.effects);
    assert!(app.pending_command().is_none());
    assert_eq!(
        service
            .list(ScopeSelection::ProjectOnly, false)
            .unwrap()
            .len(),
        1
    );

    // `y` asks the loop to run it, and the result is the notice.
    submit(&mut app, &service, &format!("/memory forget {}", record.id));
    let run = app.on_key(key(KeyCode::Char('y')));
    assert!(
        matches!(
            run.effects.as_slice(),
            [Effect::RunMemoryCommand {
                confirmed: true,
                ..
            }]
        ),
        "{:?}",
        run.effects
    );
    for effect in run.effects {
        for input in run_effect(&effect, &service) {
            app.update(input);
        }
    }
    assert!(app.pending_command().is_none());
    assert!(
        notices(&app).iter().any(|n| n.contains("forgot")),
        "{:?}",
        notices(&app)
    );
    assert!(
        service
            .list(ScopeSelection::ProjectOnly, false)
            .unwrap()
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pending_confirmation_keeps_keys_out_of_the_input_box() {
    let (dir, mut app, service) = app_with_memory("keys");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "keep".into())
        .unwrap();
    submit(&mut app, &service, &format!("/memory forget {}", record.id));
    app.on_key(key(KeyCode::Char('x')));
    assert!(
        app.input.is_empty(),
        "a pending confirmation swallows typing"
    );
    assert!(app.pending_command().is_some());
    app.on_key(key(KeyCode::Esc));
    assert!(app.pending_command().is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0025 S3: `/skill:<name>` becomes an expansion effect (the host reads the body), and its
/// answer becomes one user message — or a steer while a turn runs. An error is a notice.
#[test]
fn a_skill_command_expands_through_the_host_then_sends() {
    let (dir, mut app, service) = app_with_memory("skill");
    assert_eq!(
        submit(&mut app, &service, "/skill:greet hi there"),
        vec![Effect::ExpandSkill {
            name: "greet".into(),
            args: "hi there".into(),
            steer: false,
        }]
    );
    // The loop answers with the host's expansion; the app sends it as one user message.
    let expanded = "<skill name=\"greet\">body</skill>\n\nhi there";
    let update = app.update(Input::SkillExpanded {
        result: Ok(expanded.into()),
        steer: false,
    });
    assert_eq!(
        update.effects,
        vec![Effect::SendFollowup(agent_core::Message::user(expanded))]
    );

    // While a turn runs the same command steers.
    app.projection.set_turn(TurnState::Running);
    assert_eq!(
        submit(&mut app, &service, "/skill:greet"),
        vec![Effect::ExpandSkill {
            name: "greet".into(),
            args: String::new(),
            steer: true,
        }]
    );

    // A failed expansion is a notice and sends nothing (`/skill:nope`).
    let update = app.update(Input::SkillExpanded {
        result: Err("no skill named `nope`".into()),
        steer: true,
    });
    assert!(update.effects.is_empty(), "{:?}", update.effects);
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("no skill named `nope`")),
        "{:?}",
        notices(&app)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/skills` is an effect too: the host owns discovery, and the answer is shown as notices.
#[test]
fn a_skills_command_lists_through_the_host() {
    let (dir, mut app, service) = app_with_memory("skills");
    assert_eq!(
        submit(&mut app, &service, "/skills"),
        vec![Effect::ListSkills]
    );
    app.update(Input::SessionDone(vec!["greet: Say hello.".into()]));
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("greet: Say hello.")),
        "{:?}",
        notices(&app)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
