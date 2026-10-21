//! How a run builds its first frame: the test hooks, the resumed history, the draft,
//! the startup block and the trust modal.
//! Split from `cli.rs`'s `run()` (ADR 0043 decision 5): extractions, not pure moves.
//! No behaviour change: the block, the safety lines and the modal read the same facts
//! in the same order, and the herdr reporter is built exactly once.

use super::run_resolve::SessionCarry;
use super::*;

/// `run()`'s startup-screen phase: applies the test hooks, renders the resumed history,
/// restores the draft, builds the startup block and pushes the trust modal when the
/// workspace needs a decision. Returns the herdr reporter for the event loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_startup_screen(
    cli: &Cli,
    app: &mut App,
    session: &radian_host::LocalSession,
    history: &[agent_core::Message],
    dir: &Path,
    session_id: &str,
    resume: &Option<String>,
    workspace_root: &radian_tui::workspace::CodingRoot,
    workspace_path: &Path,
    window: &radian_host::Window,
    model_name: &str,
    has_model: bool,
    tier: radian_tui::profile::Tier,
    access_source: radian_tui::profile::AccessSource,
    config: &radian_host::Config,
    startup_approval: &radian_host::StartupApproval,
    pick_notice: bool,
    config_warnings: &[String],
    tx: &Sender<UiEvent>,
    startup_log: &mut StartupLog,
) -> Option<(HerdrReporter, Option<String>)> {
    if let Some(path) = &cli.block_input {
        let _ = BLOCK_INPUT.set(path.clone());
    }
    if let Some(path) = &cli.freeze_draw {
        let _ = FREEZE_DRAW.set(path.clone());
    }
    if let Some(millis) = cli.slow_draw {
        let _ = SLOW_DRAW.set(millis);
    }
    if let Some(count) = cli.flood_facts {
        // Test hook (`--flood-facts`): a producer that fills the channel as fast as it can, for
        // the draw-fairness test. Real producers are bounded by the turn structure; this one is
        // not, which is exactly what makes the fairness rule testable.
        let flood = tx.clone();
        std::thread::spawn(move || {
            for index in 0..count {
                let event = AgentEvent::ToolStarted {
                    index: 0,
                    call_id: format!("flood-{index}"),
                    name: "flood".into(),
                };
                if flood.send(UiEvent::Agent(event)).is_err() {
                    return;
                }
            }
        });
    }
    // `/memory` runs against the same service the agent's tools and injection use.
    app.set_memory_enabled(session.facts().memory_enabled);
    // ADR 0047: the TUI shares the host's folded goal (a resume parks an active goal as
    // paused); the `goal` tool and the prompt hook advance the same state.
    app.set_goal(session.goal());
    for message in history {
        app.push_message(message);
    }
    mark("entries-built");
    app.prefill_history(history);
    mark("prefill");
    // A draft from a previous exit is restored once, when resuming the same session.
    if resume.is_some() {
        let path = radian_tui::composer::draft_path(dir, session_id);
        match radian_tui::composer::take_draft(&path) {
            Ok(Some(text)) => app.input.set_text(&text),
            Ok(None) => {}
            Err(error) => {
                startup_log.line(format!("[draft] cannot read {}: {error}", path.display()))
            }
        }
    }
    // the startup information is one collapsible block (wordmark, one
    // information line, the safety lines that never fold, one Tip) instead of a wall of
    // notices. The full lines are what the notices used to be; the summary keeps each field
    // to a word or two.
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let workspace_line = workspace_root.note.as_ref().map(|note| {
        format!(
            "workspace: {} ({note})",
            radian_tui::workspace::display_coding_root(workspace_root, home.as_deref())
        )
    });
    // The context window (ADR 0018) and the loaded context files (ADR 0036 C2) are separate
    // lines; `context:` is the files, matching the ADR's wording.
    let context_line = format!("context window: {}", window.describe());
    let session_line = match resume {
        Some(id) => format!(
            "resumed session {id} ({} messages) · Enter sends · Ctrl-C cancels a turn · \
             Ctrl-C again quits",
            history.len()
        ),
        None => format!(
            "new session {session_id} · Enter sends · Ctrl-C cancels a turn · Ctrl-C again quits"
        ),
    };
    let sandbox_facts = session.facts().sandbox_facts.clone();
    // The host computed this from the real backend (ADR 0027): searching the description text
    // reported `none` on macOS, because the `workspace` tier's sentence does not name it.
    let sandbox_word = session.facts().sandbox_word.clone();
    let budget = window
        .budget
        .map_or_else(|| "unknown".to_string(), |b| b.to_string());
    let mut summary = format!(
        "{}{} · {sandbox_word} · context {budget} · memory {} · ask_user {}",
        tier.as_str(),
        access_source.suffix(),
        if session.facts().memory_enabled {
            "on"
        } else {
            "off"
        },
        if session.facts().ask_user {
            "on"
        } else {
            "off"
        },
    );
    if let Some(note) = &workspace_root.note {
        summary.push_str(" · ");
        summary.push_str(note);
    }
    // ADR 0034: report to herdr when this process runs inside a herdr pane. The first report
    // is the current state (idle, or blocked while the trust prompt waits) with the session
    // identity and the resume command.
    let mut herdr = herdr_reporter(
        config,
        session_id,
        session.journal_path(),
        &resume_extra_args(cli, workspace_path),
    );
    let herdr_note = herdr.as_ref().and_then(|(_, note)| note.clone());
    if let Some((reporter, _)) = herdr.as_mut() {
        reporter.maybe_report(app);
        // ADR 0034: the session identity is its own method, after the state
        // report (herdr accepts a `resume_argv` only from a reporter that already holds the pane).
        reporter.report_session();
    }
    let mut startup_lines = vec![
        format!("profile: {}", session.facts().profile_line),
        sandbox_facts,
        context_line,
        format!("context: {}", session.facts().context_line),
    ];
    // ADR 0034 H1: the startup line says reporting is on, and names a rejected resume command.
    if herdr.is_some() {
        startup_lines.push(match &herdr_note {
            Some(reason) => format!("herdr: reporting (no resume: {reason})"),
            None => "herdr: reporting".to_owned(),
        });
    }
    if let Some(line) = &workspace_line {
        startup_lines.push(line.clone());
    }
    startup_lines.push(session.facts().memory_notice.clone());
    startup_lines.push(session.facts().ask_user_line());
    // ADR 0026 D9: the startup line names the auto policy whichever way it was chosen.
    if startup_approval.policy == radian_permissions::ApprovalPolicy::Auto {
        startup_lines.push(
            "auto: tool calls are approved without asking; deny rules and the sandbox still \
             apply"
                .to_string(),
        );
    }
    if pick_notice {
        startup_lines.push("no sessions for this workspace yet; starting a new one".to_string());
    }
    // ADR 0008 §2: with no model routed, the startup block (and the empty input box) point
    // at `/model` (and `/login` when the provider needs a key).
    if !has_model {
        startup_lines.push(
            "no model is set; choose one with /model (and /login if the provider needs a key)"
                .to_string(),
        );
    }
    startup_lines.push(session_line);
    // ADR 0031 T3: the trust state, in the startup block (same style as `ask_user: off`).
    startup_lines.push(radian_host::trust::status_line(&session.trust()));
    // ADR 0047: a paused goal at startup (a `--resume` parked it, or a `/fork` carried
    // it) never auto-continues; the block names it and points at `/goal resume`.
    if let Some(line) = goal_resume_hint(session) {
        startup_lines.push(line);
    }
    // The startup diagnostics the `--pick` path could not print: the
    // alternate screen was already up, so they were collected instead (`StartupLog`). They
    // belong to the startup block, next to the trust line that was already carried here. On a
    // plain run the log collected nothing: the same lines were printed before the terminal.
    startup_lines.extend(startup_log.take());
    // The safety lines are never folded away: a workspace boundary that
    // was not the user's choice, and every config-file warning/ignored key. The config
    // warnings are also printed before the alternate screen, where a crash can still show
    // them.
    let mut safety = Vec::new();
    if let Some(line) = &workspace_line {
        safety.push(line.clone());
    }
    // The policy warnings (a config `auto` ignored under `full`, a suppressed restore) are
    // never folded either: they explain why the session's approvals behave as they do.
    for warning in &startup_approval.warnings {
        safety.push(warning.clone());
    }
    for warning in config_warnings {
        safety.push(format!("config: {warning}"));
    }
    let banner = if config.tui.banner.unwrap_or(true) {
        let path = radian_tui::workspace::display_coding_root(workspace_root, home.as_deref());
        // ADR 0008 §2: the banner names the missing model instead of a blank.
        let banner_model = if has_model { model_name } else { "no model" };
        radian_tui::startup::banner_rows(env!("CARGO_PKG_VERSION"), banner_model, &path)
    } else {
        Vec::new()
    };
    let tip = config
        .tui
        .tips
        .unwrap_or(true)
        .then(|| radian_tui::startup::tip_for(session_id).to_string());
    mark("before-startup");
    app.set_startup(radian_tui::startup::StartupScreen {
        banner,
        banner_narrow: format!("{} radian {}", app.brand_mark(), env!("CARGO_PKG_VERSION")),
        // decision 2026-10-20: while the project has no trusted decision, the
        // wordmark carries the `/trust` hint under it; trusted projects show nothing.
        trust_hint: radian_tui::startup::trust_hint(&session.trust()),
        summary,
        lines: startup_lines,
        safety,
        tip,
        expanded: false,
    });
    // ADR 0031 T3: ask once, after the terminal is up and the startup screen is built, when
    // the workspace has project resources and no saved decision. Until it is answered the
    // session is untrusted (nothing project-provided is loaded). Slice B2: the modal lists
    // the project `.mcp.json` servers that trust would load. Slice B (hooks): the modal
    // also lists the project `.radian/hooks.json` hooks that trust would load.
    if session.trust().needs_ask {
        let mcp = radian_host::preview_project(&session.trust().workspace);
        let hooks = radian_host::preview_project_hooks(&session.trust().workspace);
        app.push_trust_modal(session.trust().workspace.clone(), mcp, hooks);
    }
    herdr
}

/// ADR 0047: the startup hint for a paused goal. `None` when no goal waits: a fresh
/// session without one, a finished one, or an active one (which cannot happen at startup:
/// the resume fold parks it). A `/new` next session opens a fresh journal with no goal
/// events, so its hint is always `None`: the old goal stays durable in the old journal
/// only.
fn goal_resume_hint(session: &radian_host::LocalSession) -> Option<String> {
    let share = session.goal();
    let guard = share.lock().unwrap_or_else(|e| e.into_inner());
    let goal = guard.as_ref()?;
    if goal.status != radian_session::GoalStatus::Paused {
        return None;
    }
    let mut objective: String = goal.objective.chars().take(120).collect();
    if goal.objective.chars().count() > 120 {
        objective.push('…');
    }
    Some(format!(
        "goal paused: {objective}; /goal resume to continue"
    ))
}

/// How one session ended: the ordinary exit with its reason line, or an accepted `/new` —
/// end this session and open a fresh one in the same terminal, carrying the values in
/// force. The new session's path returns the terminal guard (never restored) so the
/// caller keeps the terminal across sessions.
pub(super) enum SessionEnd {
    Done(Option<String>),
    New(SessionNew),
}

/// An accepted `/new`: the carried values plus the unrestored terminal guard.
pub(super) struct SessionNew {
    pub(super) carry: SessionCarry,
    pub(super) guard: radian_tui::terminal::TtyGuard,
}

/// `run()`'s loop phase: builds the session host, runs the event loop to completion,
/// then (ordinary exit) restores the terminal, or (`/new`) keeps it for the next session.
/// Both paths save the draft, stop the driver and report the end: the reason line for an
/// exit, the carried values for a new session. Takes the open session by value.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_loop_and_teardown(
    cli: &Cli,
    config: &radian_host::Config,
    runtime: &tokio::runtime::Runtime,
    rx: &Receiver<UiEvent>,
    tx: &Sender<UiEvent>,
    app: &mut App,
    session: radian_host::LocalSession,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut guard: radian_tui::terminal::TtyGuard,
    memory_service: &Option<radian_host::memory::MemoryService>,
    workspace_path: &Path,
    provider_name: &str,
    model_name: &str,
    route_price: &RoutePrice,
    bash_sandbox: Option<radian_sandbox::Sandbox>,
    usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    chatgpt_usage_gate: Option<tokio::sync::watch::Sender<bool>>,
    background: Vec<tokio::task::JoinHandle<()>>,
    queue_wake: tokio::task::JoinHandle<()>,
    herdr: Option<(HerdrReporter, Option<String>)>,
    dir: &Path,
    session_id: &str,
    secrets: &radian_tui::terminal::SharedSecrets,
    has_model: bool,
) -> Result<SessionEnd, RunError> {
    // ADR 0008 revision: `/theme` writes to the file this process read, or to the path it
    // would read when there is no file yet (the write creates it).
    let theme_write_path = config
        .source
        .clone()
        .or_else(|| radian_host::resolved_config_path(cli.config.as_deref()));
    // ADR 0008 §2: the session records whether it started without a model; the first
    // `/model` writes back to this same path.
    app.set_has_model(has_model);
    app.session.model_name = model_name.to_owned();
    app.session.provider = (!model_name.is_empty()).then(|| provider_name.to_owned());
    let session_host = SessionHost {
        dir: dir.to_path_buf(),
        workspace: workspace_path.to_string_lossy().into_owned(),
        session_id: session_id.to_owned(),
        secrets: Arc::clone(secrets),
        journal: Arc::clone(session.journal()),
        config_snapshot: serde_json::json!({
            "provider": provider_name,
            "model": model_name,
        }),
        theme_colors: config.tui.colors.clone(),
        theme_write_path: theme_write_path.clone(),
        config_read_only: config.read_only,
        started_without_model: !has_model,
    };
    mark("startup-set");
    let session = Arc::new(session);
    let facade: Arc<dyn radian_host::SessionFacade> =
        Arc::clone(&session) as Arc<dyn radian_host::SessionFacade>;
    mark("before-ui-loop");
    let outcome = ui_loop(
        &mut *terminal,
        &mut *app,
        rx,
        tx,
        Phase::Session {
            workspace: workspace_path,
            runtime,
            session: &facade,
            host: &session_host,
            memory: memory_service,
            command_hooks: Some(session.command_hooks()),
        },
        (provider_name.to_owned(), model_name.to_owned()),
        Arc::clone(route_price),
        &config.providers,
        config.tui.theme.clone(),
        cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        bash_sandbox,
        usage_gate,
        chatgpt_usage_gate,
        herdr.map(|(reporter, _)| reporter),
    );
    exit_trace("loop-done");
    // Tell a pending signal task that the loop left on its own, so it does not force-exit.
    radian_tui::signals::confirm_exit();
    // `/new` keeps the terminal for the next session; every other exit restores it now,
    // exactly once, before the teardown below (a `ui_loop` error still restores first,
    // as before — `Drop` would restore anyway on the way out).
    let new_session = matches!(&outcome, Ok(LoopEnd::NewSession));
    if !new_session {
        guard.restore();
        exit_trace("restored");
    }
    // The old session's background producers and queue forwarder stop here: their ticks
    // and notices belong to the session that spawned them. A `/new` next session starts
    // its own; an ordinary exit leaves with the channel draining. (The usage gates'
    // senders drop with the loop's end, which stills their tasks the same way.)
    for task in background {
        task.abort();
    }
    queue_wake.abort();
    // The draft is written on every clean exit (normal quit, Ctrl-C, SIGTERM/SIGHUP, and
    // `/new` — the draft file is keyed by the ending session, and its resume restores it).
    // A panic never reaches here, so the panic path does no file I/O.
    let draft = radian_tui::composer::draft_path(dir, session_id);
    if let Err(error) = radian_tui::composer::save_draft(&draft, &app.input.text()) {
        eprintln!("[draft] cannot write {}: {error}", draft.display());
    }

    session.stop();
    // The driver task is the session: if it died, the failure is the app's failure. A panic
    // inside it was caught by the runtime, so `join` reports it, and the message the hook
    // recorded is printed now that the alternate screen is gone.
    let joined = runtime.block_on(session.join());
    exit_trace("stopped");
    match joined {
        radian_host::DriverEnd::Ok => {}
        radian_host::DriverEnd::Failed(error) => {
            return Err(RunError::Failed(format!("driver: {error}")));
        }
        radian_host::DriverEnd::Panicked(message) => {
            let mut message = format!("the driver task ended unexpectedly: {message}");
            if let Some(panic) = radian_tui::terminal::last_panic() {
                // One line: the normal screen gets one reason line (dogfooding D4).
                message.push_str(": ");
                message.push_str(&panic);
            }
            return Err(RunError::Failed(message));
        }
    }
    // `/new` keeps the terminal: the host ends this session and opens a fresh one in it.
    // The journal's lock releases when the last `Arc` of this session drops below.
    // A failure in the UI loop is the app's failure too; the terminal is already restored.
    let end = outcome?;
    if matches!(end, LoopEnd::NewSession) {
        exit_trace("new-session");
        return Ok(SessionEnd::New(SessionNew {
            carry: carry_from(app, &session, session_id),
            guard,
        }));
    }
    let signal = match end {
        LoopEnd::Quit(signal) => signal,
        // Handled above: the new session leaves through its own path.
        LoopEnd::NewSession => unreachable!("a `/new` exit keeps the terminal"),
    };
    let saved = format!(
        "session {session_id} saved; {}",
        resume_command(&program_name(), session_id, cli, workspace_path)
    );
    exit_trace("run-done");
    Ok(SessionEnd::Done(Some(match signal {
        Some(signal) => format!("received {}; {saved}", radian_tui::signals::name(signal)),
        None => saved,
    })))
}

/// Reads the values a `/new` carries into the fresh session: the route, thinking level,
/// tier and auto policy in force — a queued switch (applied at the next turn boundary,
/// which never comes) wins over the applied one, because it is the user's latest choice.
fn carry_from(app: &App, session: &radian_host::LocalSession, session_id: &str) -> SessionCarry {
    let route = app
        .session
        .next_model
        .as_ref()
        .and_then(|next| next.split_once('/'))
        .map(|(provider, model)| (provider.to_owned(), model.to_owned()))
        .or_else(|| session.model_route());
    let thinking = app
        .session
        .next_thinking
        .clone()
        .or_else(|| app.session.thinking_level.clone());
    let tier = app
        .session
        .projection
        .read()
        .tier()
        .and_then(|tier| tier.split_whitespace().next())
        .and_then(|name| radian_tui::profile::parse_tier(name).ok());
    SessionCarry {
        route,
        thinking,
        tier,
        auto: app.session.auto,
        old_session_id: session_id.to_owned(),
    }
}
