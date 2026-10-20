//! How a run wires the open session up: the resume/model/access overrides, the app's
//! first-frame facts, the background contributors and the usage gates (ADR 0026 D8/D9,
//! ADR 0038 decision 2, ADR 0027 C3/C4). Split from `cli.rs`'s `run()` (ADR 0043
//! decision 5): extractions, not pure moves. No behaviour change: the journal stays the
//! authority for the next resume, and every notice lands in the same order.

use super::*;

/// `run()`'s post-open wiring: records the command line's overrides, mirrors the live
/// policy/tier/price into the app, and spawns the background contributors and usage
/// gates. Returns the two usage gates for the event loop.
#[allow(clippy::too_many_arguments)]
pub(super) fn wire_post_open(
    cli: &Cli,
    config: &radian_host::Config,
    runtime: &tokio::runtime::Runtime,
    tx: &Sender<UiEvent>,
    session: &radian_host::LocalSession,
    app: &mut App,
    workspace_path: &Path,
    provider_name: &str,
    model_name: &str,
    thinking_for_footer: String,
    tier: radian_tui::profile::Tier,
    access_source: radian_tui::profile::AccessSource,
    resumed_tier: Option<radian_tui::profile::Tier>,
    cli_tier: Option<radian_tui::profile::Tier>,
    inherited_sandbox: bool,
    startup_approval: &radian_host::StartupApproval,
    window: &radian_host::Window,
    route_price: &RoutePrice,
    bash_sandbox: &Option<radian_sandbox::Sandbox>,
    command_specs: Vec<radian_host::CommandSpec>,
    session_id: &str,
    startup_log: &mut StartupLog,
) -> (
    Option<tokio::sync::watch::Sender<bool>>,
    Option<tokio::sync::watch::Sender<bool>>,
) {
    // ADR 0026 D8: an explicit `--model`/`--provider` wins over the resumed change, and the
    // override is recorded so the journal stays the authority for the next resume.
    if cli.resume.is_some()
        && let Some((recorded_provider, recorded_model)) = session.model_route()
    {
        let model_differs = cli.model.as_deref().is_some_and(|m| m != recorded_model);
        let provider_differs = cli
            .provider
            .as_deref()
            .is_some_and(|p| p != recorded_provider);
        if model_differs || provider_differs {
            startup_log.line(format!(
                "radian: resume recorded {recorded_provider}/{recorded_model}; using the command \
                 line's {provider_name}/{model_name}"
            ));
            session
                .queue_config(radian_host::ModelRoute::new(provider_name, model_name).to_value());
        }
    }
    // ADR 0026 D9: an explicit command-line choice is recorded, so a resume restores it. The
    // driver journals the change at the next turn boundary, revision after the journal's.
    // The hooks already carry the policy (the profile above), so this is the record only —
    // no audit line: nothing switched inside the session.
    if let Some(policy) = startup_approval.backfill {
        session
            .config_queue()
            .push(serde_json::json!({ "approval": policy.as_str() }));
    }
    // ADR 0038 decision 2 (same rule as /model D8): an explicit `--profile` on a resume wins
    // over the journal's recorded access, and the override is recorded so the journal stays
    // the authority for the next resume.
    if cli.resume.is_some() && cli.profile.is_some() && resumed_tier != cli_tier {
        if let Some(recorded) = resumed_tier {
            startup_log.line(format!(
                "radian: resume recorded tier {}; using the command line's {}",
                recorded.as_str(),
                tier.as_str()
            ));
        }
        session
            .config_queue()
            .push(serde_json::json!({ "access": tier.as_str() }));
    }
    // The status bar mirrors the live policy from the first frame; the switch below is the
    // only other writer.
    app.set_auto(startup_approval.policy == radian_permissions::ApprovalPolicy::Auto);
    // ADR 0038 decision 3: `/access` and the footer show the tier and, when it came from the
    // remembered record, the `(project)` marker. The host set the projection's tier when it
    // opened the session; the marker is added here.
    app.set_access_source(access_source);
    app.set_tier(radian_tui::profile::footer_tier(tier, access_source));
    // ADR 0016: `/access` repeats the startup line's own words for an
    // inherited sandbox (and says nothing extra for any other tier).
    if inherited_sandbox {
        app.set_sandbox_note(Some(session.facts().profile_line.clone()));
    }
    // The footer's startup facts (ADR 0027 C2): the working directory, the model's display
    // name (the id until a catalog name is known), the provider and the applied thinking
    // level. The git branch is not read here: the `git` contributor refreshes it in the
    // background (ADR 0027 C3), and the draw path never runs git.
    app.set_footer_facts(
        Some(workspace_path.to_path_buf()),
        None,
        Some(provider_name.to_owned()),
        Some(thinking_for_footer),
    );
    // ADR 0027: the hostname, the model's catalog prices (`cost`) and whether
    // automatic compaction is on (`context`'s `{auto}`). Each is a startup fact.
    app.set_hostname(radian_host::hostname());
    // The price must be the one for the route actually in force: a resume where the journal
    // recorded a different model uses **that** model's rate, not the command line's (ADR 0026
    // D8). `window.price` is the startup route's (possibly freshly fetched); a differing
    // resumed route is resolved from the warm cache only, like any other switch.
    let startup_price = match session.model_route() {
        Some((route_provider, route_model))
            if (route_provider.as_str(), route_model.as_str()) != (provider_name, model_name) =>
        {
            route_price(&route_provider, &route_model)
        }
        _ => window.price.clone(),
    };
    app.set_model_price(startup_price);
    app.set_bash_available(bash_sandbox.is_some());
    app.set_auto_compact(!cli.no_auto_compact && config.compaction.auto.unwrap_or(true));
    // ADR 0027 C3: the contributors. Their changes wake the UI through the same dirty channel
    // (ADR 0023 U2), and a refresh failure becomes a notice; the UI never waits for one.
    let contributions = radian_host::ContributionStore::new();
    let contrib_tx = tx.clone();
    let on_tick: Arc<dyn Fn(radian_host::Tick) + Send + Sync> = Arc::new(move |tick| {
        let _ = contrib_tx.send(UiEvent::Dirty);
        if let Some(notice) = tick.notice {
            let _ = contrib_tx.send(UiEvent::Command(AppInput::SessionDone(vec![notice])));
        }
    });
    radian_host::spawn_contributor(
        runtime.handle(),
        Arc::new(radian_host::GitContributor::new(workspace_path)),
        Arc::clone(&contributions),
        Arc::clone(&on_tick),
    );
    // ADR 0027 C4 + revision 5 + revision 14 decision 3: the provider's usage source comes
    // from its descriptor (`usage = Endpoint`). It is spawned only when a key is stored, but it
    // refreshes (and its row shows) only while the **current** model's provider is
    // opencode-go. The watch channel is that gate: the UI flips it on a `/model` switch. The
    // key never leaves this spawn: only the snapshot enters the store.
    // `usage_period_secs = 0` turns it off; the period is clamped to 15 s–10 min.
    let usage_gate = if let Some(period) = usage_period(config.tui.usage_period_secs)
        && let Ok(Some(key)) = radian_host::auth::resolve_key("opencode-go")
        && let Some(endpoint) = radian_host::usage_endpoint("opencode-go")
    {
        // The startup model decides the first state (a resumed route wins over the CLI's).
        let startup_provider = session
            .model_route()
            .map(|(provider, _)| provider)
            .unwrap_or_else(|| provider_name.to_owned());
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(startup_provider == "opencode-go");
        radian_host::spawn_usage_gated(
            runtime.handle(),
            (endpoint.build)(&radian_host::UsageArgs {
                url: endpoint.url.to_owned(),
                session_id: session_id.to_owned(),
                secret: Some(key.key.clone()),
            }),
            period,
            Arc::clone(&contributions),
            Arc::clone(&on_tick),
            gate_rx,
        );
        Some(gate_tx)
    } else {
        None
    };
    // The same mechanism for `chatgpt` (ADR 0026), gated on
    // `has_login("chatgpt")` rather than a resolved api_key (there is none — the source
    // refreshes the oauth entry itself, per call, exactly like `ChatGptModel`).
    let chatgpt_usage_gate = if let Some(period) = usage_period(config.tui.usage_period_secs)
        && radian_host::has_login("chatgpt")
        && let Some(endpoint) = radian_host::usage_endpoint(radian_host::CHATGPT_PROVIDER_ID)
    {
        let startup_provider = session
            .model_route()
            .map(|(provider, _)| provider)
            .unwrap_or_else(|| provider_name.to_owned());
        let (gate_tx, gate_rx) =
            tokio::sync::watch::channel(chatgpt_usage_active(&startup_provider));
        radian_host::spawn_usage_gated(
            runtime.handle(),
            (endpoint.build)(&radian_host::UsageArgs {
                url: endpoint.url.to_owned(),
                session_id: session_id.to_owned(),
                secret: None,
            }),
            period,
            Arc::clone(&contributions),
            Arc::clone(&on_tick),
            gate_rx,
        );
        Some(gate_tx)
    } else {
        None
    };
    app.set_contributions(Arc::clone(&contributions));
    // ADR 0027: the out-of-process footer `command` items. Each runs on
    // its own background producer; its value enters the contribution store and nowhere else, so
    // the renderer needs no special case. The command's stdin facts come from a snapshot the loop
    // publishes (a safe handoff, never the app itself), published once here before the first run
    // and again before every frame.
    let footer_context = radian_host::FooterContext::new();
    footer_context.set(app.footer_snapshot());
    app.set_footer_context(Arc::clone(&footer_context));
    for spec in command_specs {
        let once = spec.period.is_none();
        let contributor = Arc::new(radian_host::CommandContributor::new(
            spec,
            Arc::clone(&footer_context),
            session_id.to_owned(),
            Some(workspace_path.to_string_lossy().into_owned()),
        ));
        if once {
            radian_host::spawn_contributor_once(
                runtime.handle(),
                contributor,
                Arc::clone(&contributions),
                Arc::clone(&on_tick),
            );
        } else {
            radian_host::spawn_contributor(
                runtime.handle(),
                contributor,
                Arc::clone(&contributions),
                Arc::clone(&on_tick),
            );
        }
    }
    (usage_gate, chatgpt_usage_gate)
}

/// Everything the event loop runs with: the entered terminal, the session's memory
/// service and the recovered transcript. One struct so `run()` threads a name, not
/// three locals.
pub(super) struct TerminalReady {
    pub(super) guard: radian_tui::terminal::TtyGuard,
    pub(super) terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    pub(super) memory_service: Option<radian_host::memory::MemoryService>,
    pub(super) history: Vec<agent_core::Message>,
}

/// `run()`'s terminal phase: enters the terminal on a plain run (the `--pick` path
/// already entered it), subscribes the queue's wake-ups, runs the panic-path test hooks
/// and takes the recovered transcript. The panics diverge like the inline code did.
#[allow(clippy::too_many_arguments)]
pub(super) fn enter_terminal_and_history(
    cli: &Cli,
    runtime: &tokio::runtime::Runtime,
    tx: &Sender<UiEvent>,
    dir: &Path,
    mouse: bool,
    startup_terminal: Option<(
        radian_tui::terminal::TtyGuard,
        Terminal<CrosstermBackend<std::io::Stdout>>,
    )>,
    session: &mut radian_host::LocalSession,
) -> Result<TerminalReady, RunError> {
    // A plain run enters the terminal only now: every fallible check above, and the session
    // itself, have already run. (`--pick` entered it before the picker and keeps it here.)
    let (guard, terminal) = match startup_terminal {
        Some(pair) => pair,
        None => {
            let pair = enter_terminal(mouse)?;
            spawn_input_thread(tx.clone());
            pair
        }
    };
    mark("terminal-ready");
    // ADR 0023 U7: the queue's changes are a wake-up source. A push, an answer, an abort and
    // a waiter that gave up all bump it, so the UI re-reads the head and the modal matches the
    // queue even when nothing else happened.
    {
        let mut changes = session.queue().subscribe();
        let pending_tx = tx.clone();
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                if pending_tx.send(UiEvent::Pending).is_err() {
                    break;
                }
            }
        });
    }
    if cli.panic_after_enter {
        panic!("radian: panic path test");
    }
    if let Some(message) = &cli.panic_message {
        panic!("{message}");
    }
    if let Some(secs) = cli.panic_with_child {
        // Test hook: a child in its own process group is running when the panic happens.
        // The panic path must leave no orphan behind.
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let mut command = tokio::process::Command::new("sleep");
            command
                .arg(secs.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            #[cfg(unix)]
            command.process_group(0);
            let mut child = command.spawn().expect("spawn the test child");
            let _ = pid_tx.send(child.id().unwrap_or(0));
            let _ = child.wait().await;
        });
        let pid = runtime
            .block_on(pid_rx)
            .map_err(|_| "the test child did not start".to_string())?;
        std::fs::write(dir.join("child.pid"), pid.to_string())
            .map_err(|e| format!("child.pid: {e}"))?;
        panic!("radian: panic path test with a child in flight");
    }
    if let Some(command) = &cli.panic_with_background_job {
        // Test hook: a background job is running when the panic happens. The panic path must
        // kill its whole process group (ADR 0037 decision 6). The registry is reached through
        // the host facade; the command runs under a `full` sandbox (this is a test hook).
        let jobs = session
            .jobs_registry()
            .expect("the session has coding tools");
        let workspace = session.workspace().to_path_buf();
        let policy =
            radian_sandbox::SandboxPolicy::new(radian_sandbox::SandboxMode::FullAccess, &workspace)
                .expect("a full-access policy");
        let sandbox = radian_sandbox::Sandbox::from_policy(policy).expect("a full-access sandbox");
        jobs.start(&sandbox, &workspace, command)
            .expect("start the test job");
        // The command writes its pid (and its child's) before it sleeps; wait for it, bounded.
        let marker = workspace.join("job.pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::fs::read_to_string(&marker).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the test job never wrote job.pid"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("radian: panic path test with a background job in flight");
    }
    let memory_service = session.memory().cloned();
    // The history of a resumed session is rendered before the first frame, so the transcript
    // shows it from the start. `open_session` already recovered exactly this transcript
    // (`agent_core::recover` over the same journal records `Session::history` folds), so the
    // host takes it instead of re-opening and re-folding the file (resume-perf B).
    let history = std::mem::take(&mut session.history);

    mark("history-read");
    Ok(TerminalReady {
        guard,
        terminal,
        memory_service,
        history,
    })
}
