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
    app.session
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice)
        .map(|entry| entry.text.clone())
        .collect()
}

/// Runs one effect the way the event loop does, for the arms these tests need, and returns the
/// inputs its result produces. A `/memory import <path>` reads the file here, mirroring the
/// loop (`LoopContext::execute`): a relative path resolves against `workspace`, an absolute
/// path may point anywhere, and an unreadable file becomes the CLI's `cannot read` failure
/// without running the command.
fn run_effect(workspace: &std::path::Path, effect: &Effect, service: &MemoryService) -> Vec<Input> {
    match effect {
        Effect::RunMemoryCommand { command, confirmed } => {
            let command = match command.clone() {
                radian_host::memory::commands::MemoryCommand::Import {
                    global,
                    project,
                    source,
                } if source != "-" => {
                    match radian_tui::command::read_memory_import(workspace, &source) {
                        Ok(text) => radian_host::memory::commands::MemoryCommand::Import {
                            global,
                            project,
                            source: text,
                        },
                        Err(notice) => {
                            return vec![Input::MemoryCommandDone(MemoryResult::Failed(notice))];
                        }
                    }
                }
                other => other,
            };
            let json = command.json();
            let result = match radian_host::memory::commands::execute(
                service,
                command.clone(),
                *confirmed,
                json,
            ) {
                Ok(output) => match output.needs_confirmation {
                    Some(prompt) => MemoryResult::NeedsConfirmation { command, prompt },
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
/// (the key's own, plus the ones the results produced). `dir` is the scratch root whose `ws`
/// child is the session workspace the loop resolves relative import paths against.
fn submit(
    app: &mut App,
    service: &MemoryService,
    dir: &std::path::Path,
    line: &str,
) -> Vec<Effect> {
    let workspace = dir.join("ws");
    app.input = line.into();
    let mut seen = Vec::new();
    let mut queue: VecDeque<Effect> = app.on_key(key(KeyCode::Enter)).effects.into();
    while let Some(effect) = queue.pop_front() {
        seen.push(effect.clone());
        for input in run_effect(&workspace, &effect, service) {
            queue.extend(app.update(input).effects);
        }
    }
    seen
}

#[test]
fn help_and_unbalanced_quotes_are_notices_and_send_nothing() {
    let (dir, mut app, service) = app_with_memory("help");
    assert!(submit(&mut app, &service, &dir, "/help").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("/memory")),
        "{:?}",
        notices(&app)
    );

    assert!(submit(&mut app, &service, &dir, "/unknown").is_empty());
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.contains("unknown command /unknown")),
        "{:?}",
        notices(&app)
    );

    assert!(submit(&mut app, &service, &dir, "/memory add \"unbalanced").is_empty());
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
        submit(&mut app, &service, &dir, "//hi"),
        vec![
            Effect::ExpandMentions { text: "/hi".into() },
            Effect::SendFollowup(radian_core::Message::user("/hi"))
        ]
    );
    app.session.projection.set_turn(TurnState::Running);
    assert_eq!(
        submit(&mut app, &service, &dir, "//steer me"),
        vec![
            Effect::ExpandMentions {
                text: "/steer me".into()
            },
            Effect::Steer(radian_core::Message::user("/steer me"))
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
    let effects = submit(&mut app, &service, &dir, "/memory list");
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
    app.session.projection.set_turn(TurnState::Running);
    let effects = submit(&mut app, &service, &dir, "/memory list");
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

    let effects = submit(
        &mut app,
        &service,
        &dir,
        &format!("/memory forget {}", record.id),
    );
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
    submit(
        &mut app,
        &service,
        &dir,
        &format!("/memory forget {}", record.id),
    );
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
        for input in run_effect(&dir.join("ws"), &effect, &service) {
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

/// A bare `-` is the CLI's stdin marker; the interface has no stdin, so
/// `/memory add -` writes no record, shows the one notice, and keeps the line.
#[test]
fn a_bare_dash_add_writes_nothing_and_keeps_the_line() {
    let (dir, mut app, service) = app_with_memory("dash-add");
    let effects = submit(&mut app, &service, &dir, "/memory add -");
    assert!(effects.is_empty(), "no effect may run: {effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "`-` reads stdin only in `radian memory`; type the text here"),
        "{:?}",
        notices(&app)
    );
    assert_eq!(app.input.text(), "/memory add -", "the typed line is kept");
    assert!(
        service
            .list(ScopeSelection::ProjectOnly, false)
            .unwrap()
            .is_empty(),
        "no record was written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/memory edit <id> -` leaves the record unchanged, with the same notice.
#[test]
fn a_bare_dash_edit_leaves_the_record_unchanged() {
    let (dir, mut app, service) = app_with_memory("dash-edit");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "original".into())
        .unwrap();
    let effects = submit(
        &mut app,
        &service,
        &dir,
        &format!(
            "/memory edit --revision {} {} -",
            record.revision, record.id
        ),
    );
    assert!(effects.is_empty(), "no effect may run: {effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "`-` reads stdin only in `radian memory`; type the text here"),
        "{:?}",
        notices(&app)
    );
    let found = service
        .get(ScopeSelection::ProjectOnly, &record.id)
        .unwrap();
    assert_eq!(found.content, "original");
    assert_eq!(found.revision, record.revision);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/memory import -` imports nothing and names a file path instead.
#[test]
fn a_bare_dash_import_imports_nothing() {
    let (dir, mut app, service) = app_with_memory("dash-import");
    let effects = submit(&mut app, &service, &dir, "/memory import -");
    assert!(effects.is_empty(), "no effect may run: {effects:?}");
    assert!(
        notices(&app)
            .iter()
            .any(|n| n == "`-` reads stdin only in `radian memory`; give a file path here"),
        "{:?}",
        notices(&app)
    );
    assert!(
        service
            .list(ScopeSelection::ProjectOnly, true)
            .unwrap()
            .is_empty(),
        "nothing was imported"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/memory import <relative-path>` reads the file against the session workspace and imports
/// its records (the loop reads, never `update`, so the draw loop never blocks on disk).
#[test]
fn a_relative_import_path_reads_from_the_workspace() {
    let (dir, mut app, service) = app_with_memory("import-relative");
    let ws = dir.join("ws");
    std::fs::write(
        ws.join("mem.md"),
        "---\nid: imported-relative\nrevision: 1\n---\nhello from import\n",
    )
    .unwrap();
    let effects = submit(&mut app, &service, &dir, "/memory import mem.md");
    assert!(
        matches!(effects.as_slice(), [Effect::RunMemoryCommand { .. }]),
        "the import still runs: {effects:?}"
    );
    assert!(
        notices(&app).iter().any(|n| n.contains("created 1")),
        "{:?}",
        notices(&app)
    );
    let records = service.list(ScopeSelection::ProjectOnly, false).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].content, "hello from import");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An absolute import path works too, pointing anywhere.
#[test]
fn an_absolute_import_path_works() {
    let (dir, mut app, service) = app_with_memory("import-absolute");
    let outside = scratch("import-absolute-outside");
    let path = outside.join("mem.md");
    std::fs::write(
        &path,
        "---\nid: imported-absolute\nrevision: 1\n---\nhello from import\n",
    )
    .unwrap();
    let effects = submit(
        &mut app,
        &service,
        &dir,
        &format!("/memory import {}", path.display()),
    );
    assert!(
        matches!(effects.as_slice(), [Effect::RunMemoryCommand { .. }]),
        "the import still runs: {effects:?}"
    );
    assert!(
        notices(&app).iter().any(|n| n.contains("created 1")),
        "{:?}",
        notices(&app)
    );
    let records = service.list(ScopeSelection::ProjectOnly, false).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].content, "hello from import");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

/// A missing import file writes nothing and shows the CLI's `cannot read` notice.
#[test]
fn a_missing_import_file_writes_nothing_and_shows_cannot_read() {
    let (dir, mut app, service) = app_with_memory("import-missing");
    let effects = submit(&mut app, &service, &dir, "/memory import no-such-file.md");
    assert!(
        matches!(effects.as_slice(), [Effect::RunMemoryCommand { .. }]),
        "the read happens in the loop's effect arm: {effects:?}"
    );
    assert!(
        notices(&app)
            .iter()
            .any(|n| n.starts_with("cannot read no-such-file.md: ")),
        "{:?}",
        notices(&app)
    );
    assert!(
        service
            .list(ScopeSelection::ProjectOnly, true)
            .unwrap()
            .is_empty(),
        "nothing was written"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `-` among other words is ordinary text: `/memory add "a - b"` stores `a - b`.
/// (Unquoted extra words are a clap usage error, unchanged by the dash guard.)
#[test]
fn a_dash_among_other_words_is_stored_verbatim() {
    let (dir, mut app, service) = app_with_memory("dash-words");
    let effects = submit(&mut app, &service, &dir, "/memory add \"a - b\"");
    assert!(
        matches!(effects.as_slice(), [Effect::RunMemoryCommand { .. }]),
        "a non-bare dash still runs: {effects:?}"
    );
    let records = service.list(ScopeSelection::ProjectOnly, false).unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].content, "a - b");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_pending_confirmation_keeps_keys_out_of_the_input_box() {
    let (dir, mut app, service) = app_with_memory("keys");
    let record = service
        .add(WriteScope::Project, Kind::Fact, None, "keep".into())
        .unwrap();
    submit(
        &mut app,
        &service,
        &dir,
        &format!("/memory forget {}", record.id),
    );
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
        submit(&mut app, &service, &dir, "/skill:greet hi there"),
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
        vec![Effect::SendFollowup(radian_core::Message::user(expanded))]
    );

    // While a turn runs the same command steers.
    app.session.projection.set_turn(TurnState::Running);
    assert_eq!(
        submit(&mut app, &service, &dir, "/skill:greet"),
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

/// ADR 0025 S3, built-ins `init` and `review`: `/skill:init` and `/skill:review <target>`
/// parse like `/skill:greet` — an expansion effect whose answer becomes one user message.
/// The bodies themselves are pinned host-side
/// (`the_builtin_init_and_review_skills_are_discovered_listed_and_expandable`); this pins
/// the TUI half: the command reaches the host with the right name and args.
#[test]
fn skill_init_and_review_commands_expand_through_the_host_then_send() {
    let (dir, mut app, service) = app_with_memory("skill-builtins");
    // No args: the block stands alone.
    assert_eq!(
        submit(&mut app, &service, &dir, "/skill:init"),
        vec![Effect::ExpandSkill {
            name: "init".into(),
            args: String::new(),
            steer: false,
        }]
    );
    let expanded = "<skill name=\"init\">body</skill>";
    let update = app.update(Input::SkillExpanded {
        result: Ok(expanded.into()),
        steer: false,
    });
    assert_eq!(
        update.effects,
        vec![Effect::SendFollowup(radian_core::Message::user(expanded))]
    );

    // With a target: it travels as the args (`$ARGUMENTS` in the `review` body).
    assert_eq!(
        submit(&mut app, &service, &dir, "/skill:review main"),
        vec![Effect::ExpandSkill {
            name: "review".into(),
            args: "main".into(),
            steer: false,
        }]
    );
    let expanded = "<skill name=\"review\">body</skill>";
    let update = app.update(Input::SkillExpanded {
        result: Ok(expanded.into()),
        steer: false,
    });
    assert_eq!(
        update.effects,
        vec![Effect::SendFollowup(radian_core::Message::user(expanded))]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/skills` is an effect too: the host owns discovery, and the answer is shown as notices.
#[test]
fn a_skills_command_lists_through_the_host() {
    let (dir, mut app, service) = app_with_memory("skills");
    assert_eq!(
        submit(&mut app, &service, &dir, "/skills"),
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

/// `/jobs` (ADR 0037 decision 4, inspector panel): a bare `/jobs` opens the inspector's
/// jobs page; `kill` forms stay text notices on the same path as before. The parser maps
/// the forms.
#[test]
fn a_jobs_command_lists_or_kills_through_the_host() {
    let (dir, mut app, service) = app_with_memory("jobs");
    assert_eq!(
        submit(&mut app, &service, &dir, "/jobs"),
        vec![Effect::OpenInspector {
            page: Some(radian_tui::command::InspectorPage::Jobs)
        }]
    );
    assert_eq!(
        submit(&mut app, &service, &dir, "/jobs kill job-1"),
        vec![Effect::Jobs(radian_tui::command::JobsRequest::Kill(
            "job-1".into()
        ))]
    );
    assert_eq!(
        submit(&mut app, &service, &dir, "/jobs kill all"),
        vec![Effect::Jobs(radian_tui::command::JobsRequest::KillAll)]
    );
    // Anything else is a usage notice, not an effect.
    assert!(submit(&mut app, &service, &dir, "/jobs nope").is_empty());
    assert!(
        notices(&app).iter().any(|n| n.contains("usage: /jobs")),
        "{:?}",
        notices(&app)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/jobs` lists only running background jobs (ADR 0037 decision 4, jobs-running-only): a
/// finished background job and a settled foreground retention (decision 3a) stay out of the
/// list but remain readable through the `job` tool. The notices come from
/// [`radian_tui::command::jobs_list_notices`], the same function the event loop calls for
/// `JobsRequest::List`, so removing the filter fails this test.
#[test]
fn jobs_list_shows_only_running_background_jobs() {
    use std::time::Duration;

    use radian_coding::{JobState, JobSummary};

    let summary = |id: &str, state: JobState, command: &str| JobSummary {
        id: id.to_owned(),
        command: command.to_owned(),
        state,
        elapsed: Duration::from_secs(7),
        output: radian_coding::OUTPUT_HINT.to_owned(),
    };
    let jobs = vec![
        summary("job-1", JobState::Running, "sleep 60"),
        summary("job-2", JobState::Exited(0), "echo done"),
        summary("job-3", JobState::TimedOut, "grep -r x /"),
    ];
    assert_eq!(
        radian_tui::command::jobs_list_notices(&jobs),
        vec!["job-1  running  7s  sleep 60  available through job".to_owned()]
    );
    // Settled entries only (a finished background job and a foreground retention): one
    // line, not the entries and not the old "no background jobs in this session".
    assert_eq!(
        radian_tui::command::jobs_list_notices(&jobs[1..]),
        vec!["no running background jobs".to_owned()]
    );
    assert_eq!(
        radian_tui::command::jobs_list_notices(&[]),
        vec!["no running background jobs".to_owned()]
    );
}

/// The footer's `reasoning` item shows the level that takes effect, not the configured one
/// (ADR 0026 D11): `deepseek-v4.1-flash` supports low/high/max, so a configured `medium` shows
/// `high`.
#[test]
fn the_footer_thinking_level_is_the_clamped_one() {
    let provider = radian_host::ProviderRef::BuiltIn(
        radian_host::descriptor("opencode-go").expect("the descriptor"),
    );
    let level = |requested: &str| {
        radian_tui::command::effective_thinking_level(
            &provider,
            "deepseek-v4.1-flash",
            requested,
            4096,
        )
    };
    assert_eq!(level("medium"), "high", "medium clamps up to high");
    assert_eq!(level("low"), "low");
    assert_eq!(
        level("off"),
        "low",
        "off clamps up to the lowest supported level"
    );
    // A model the descriptor does not list falls back to the provider-level map; a route with
    // no map at all keeps the requested level (nothing to clamp against).
    assert_eq!(level("medium"), "high");
}

/// `/thinking`'s rows name the wire value each level sends and mark the applied level (ADR 0026
/// D11). `deepseek-v4.1-flash` supports low/high/max; Anthropic sends `enabled:<budget>`.
#[test]
fn thinking_candidates_name_the_wire_value_and_mark_the_current_level() {
    let opencode =
        radian_host::ProviderRef::BuiltIn(radian_host::descriptor("opencode-go").unwrap());
    let rows =
        radian_tui::command::thinking_candidates(&opencode, "deepseek-v4.1-flash", 4096, "high");
    let inserts: Vec<&str> = rows.iter().map(|row| row.insert.as_str()).collect();
    assert_eq!(inserts, ["low", "high", "max"], "{rows:?}");
    let high = rows.iter().find(|row| row.insert == "high").unwrap();
    assert!(
        high.current,
        "the applied level carries `current`: {high:?}"
    );
    let low = rows.iter().find(|row| row.insert == "low").unwrap();
    assert!(!low.current, "only the applied level is current: {low:?}");
    assert!(
        !rows.iter().any(|row| row.description.contains("● current")),
        "the marker is never in the description: {rows:?}"
    );

    let anthropic =
        radian_host::ProviderRef::BuiltIn(radian_host::descriptor("anthropic").unwrap());
    let model = radian_host::descriptor("anthropic").unwrap().models[0].id;
    let rows = radian_tui::command::thinking_candidates(&anthropic, model, 16384, "high");
    let high = rows.iter().find(|row| row.insert == "high").unwrap();
    assert!(
        high.description.contains("enabled:4096"),
        "the Anthropic wire value is the budget: {}",
        high.description
    );
    assert!(high.current, "{high:?}");
}

/// The exact-match rule on a **`Ready`** level, which `is_exact_candidate` (`command.rs`) also
/// serves: a typed word that is already a candidate is not a completion to accept, even when a
/// longer candidate shares its prefix. The `Needs` half of the same rule is
/// `an_exact_model_route_is_not_armed_even_with_a_prefix_sibling` (`tests/update.rs`), where
/// `chatgpt`'s `gpt-5.4` / `gpt-5.4-mini` pair hit it. `complete_in` takes the tree as a
/// parameter exactly so a level with such a pair can be exercised here: the built-in tree has
/// none.
#[test]
fn a_ready_level_with_a_prefix_sibling_does_not_arm_on_an_exact_word() {
    fn pair(_typed: &[&str], _partial: &str) -> radian_tui::command::Level {
        let candidate = |insert: &str| radian_tui::command::Candidate {
            insert: insert.into(),
            description: String::new(),
            current: false,
        };
        radian_tui::command::Level::Ready(vec![candidate("gpt-5.4"), candidate("gpt-5.4-mini")])
    }
    static LEVELS: &[radian_tui::command::LevelSource] = &[pair];
    static TREE: &[radian_tui::command::Command] = &[radian_tui::command::Command {
        name: "model",
        description: "switch the model",
        levels: LEVELS,
    }];

    // Exactly a candidate: nothing to complete, so Enter submits the line.
    assert!(
        radian_tui::command::complete_in(TREE, "/model gpt-5.4", 14).is_none(),
        "an exact word must not arm the menu"
    );
    // A partial word still lists the sibling (only the longer id matches `gpt-5.4-m`).
    let level = radian_tui::command::complete_in(TREE, "/model gpt-5.4-m", 16).expect("a list");
    match level.level_result {
        radian_tui::command::Level::Ready(candidates) => {
            let inserts: Vec<&str> = candidates
                .iter()
                .map(|candidate| candidate.insert.as_str())
                .collect();
            assert_eq!(inserts, ["gpt-5.4-mini"], "{candidates:?}");
        }
        other => panic!("expected `Ready`: {other:?}"),
    }
}
