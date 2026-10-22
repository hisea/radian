//! How a run prepares the session's inputs: the sandbox, the approval policy, the event
//! sinks and the project trust (ADR 0012, ADR 0023 U2/U5/U7, ADR 0031 T3). Split from
//! `cli.rs`'s `run()` (ADR 0043 decision 5): extractions, not pure moves. No behaviour
//! change: every refusal still happens before the session file exists, sinks are wired
//! in the same order.

use super::*;

/// Everything `run()`'s input phase builds for `open_session`: the sandbox and its
/// rebuild builder, the approval policy, the control port, the event sinks and the
/// trust setup. The control socket guard is returned separately so `run()` holds it
/// for the whole session (ADR 0043).
pub(super) struct SessionInputs {
    pub(super) sandbox: Option<radian_sandbox::Sandbox>,
    pub(super) inherited_sandbox: bool,
    pub(super) access_builder: radian_host::AccessBuilder,
    pub(super) bash_sandbox: Option<radian_sandbox::Sandbox>,
    pub(super) servers: Vec<agent_extensions::McpServerConfig>,
    pub(super) startup_approval: radian_host::StartupApproval,
    pub(super) profile: radian_permissions::Profile,
    pub(super) projection: std::sync::Arc<radian_host::SessionProjection>,
    pub(super) control: Option<std::sync::Arc<radian_host::ControlServer>>,
    pub(super) live: std::sync::Arc<dyn EventSink>,
    pub(super) approvals: radian_host::ApprovalRouting,
    pub(super) audit:
        std::sync::Arc<dyn Fn(Arc<SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync>,
    pub(super) runtime_events: std::sync::Arc<dyn radian_runtime::RuntimeEvents>,
    pub(super) driver_stopped: std::sync::Arc<dyn Fn(radian_host::DriverEnd) + Send + Sync>,
    pub(super) before_driver: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    pub(super) memory_warning: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    pub(super) hooks_warning: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    /// A wake-budget degrade's notice line: one transcript notice per burst, plus the
    /// herdr `wake budget` block while the notices wait.
    pub(super) wake_warning: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    /// The intent-stop cap's notice line (backlog `turn-end-continuation.md` §B.3).
    pub(super) intent_stop_warning: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    /// A lost job-notice's line: the burst's transcript notice, plus the herdr
    /// `job notice lost` block while it waits (ADR 0037 decision 5's failure rule).
    pub(super) notice_lost_warning: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    pub(super) trust_setup: radian_host::trust::Setup,
    /// The project-level hooks (`<workspace>/.radian/hooks.json`, ADR 0045 slice B), empty
    /// when untrusted. Loaded once trust is resolved, like the project `.mcp.json`.
    pub(super) project_hooks: radian_host::config::HooksConfig,
    /// Where the memory service lives (`None` when no RADIAN_HOME/HOME): the session
    /// shares it with the agent's tools and injection.
    pub(super) memory_root: Option<PathBuf>,
}

/// `run()`'s input phase: the sandbox (and its deny-read list), the `/access` rebuild
/// builder, the MCP servers, the approval policy, the panic log, the projection, the
/// control port, the event sinks and the project trust. Returns the inputs plus the
/// control socket guard, which the caller holds for the whole session.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_session_inputs(
    cli: &Cli,
    config: &radian_host::Config,
    runtime: &tokio::runtime::Runtime,
    tx: &Sender<UiEvent>,
    dir: &Path,
    workspace_path: &Path,
    app: &mut App,
    resume: &Option<String>,
    tier: radian_tui::profile::Tier,
    carry_auto: bool,
    allow_unix_sockets: &[PathBuf],
    session_id: &str,
    secrets: &radian_tui::terminal::SharedSecrets,
    startup_log: &mut StartupLog,
) -> Result<(SessionInputs, Option<radian_host::SocketGuard>), RunError> {
    // The permission tier and the sandbox. For an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 P5). Still before
    // the session file exists, so a refusal leaves nothing behind (dogfooding D2).
    // The tier and its source were resolved above (ADR 0038 decision 3).
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory (plus the credential, trust and remembered-tier stores, ADR 0026 D4 / ADR 0031
    // T2 / ADR 0038 decision 3). The shared helper builds that list — the same list the file tools deny in the
    // `full` tier — so the two can never differ (ADR 0039 decision 4). The directories were
    // created owner-only before the terminal was entered (`ensure_private_dir` above);
    // Linux's bwrap can only mask paths that exist.
    let memory_root = radian_host::memory_dir();
    let sandbox_deny_read = radian_host::file_tool_deny_read(dir);
    let sandbox = radian_tui::profile::build_sandbox(
        tier,
        workspace_path,
        &sandbox_deny_read,
        allow_unix_sockets,
        &config.sandbox.allow_env,
    )?;
    // ADR 0016: when the sandbox is one radian inherited from outside,
    // `/access` says so like the startup line does. Read here, while the sandbox is still local
    // (it moves into `open_session` below).
    let inherited_sandbox = sandbox
        .as_ref()
        .is_some_and(|sandbox| sandbox.backend_name() == "inherited");
    // ADR 0038 slice 2: `/access` rebuilds the profile and the sandbox for a new tier through
    // this builder. It captures the same inputs the startup build used, so a switch and a
    // startup cannot drift; a failed sandbox probe is the `Err` that refuses the switch.
    let access_builder: radian_host::AccessBuilder = {
        let workspace = workspace_path.to_path_buf();
        let allow_shell = cli.allow_shell_command.clone();
        let deny_read = sandbox_deny_read.clone();
        let sockets = allow_unix_sockets.to_vec();
        let allow_env = config.sandbox.allow_env.clone();
        std::sync::Arc::new(move |preset: radian_permissions::Preset| {
            radian_tui::profile::build_access(
                preset,
                &workspace,
                &allow_shell,
                &deny_read,
                &sockets,
                &allow_env,
            )
        })
    };
    // ADR 0033: the user's `!`/`!!` commands run in the **same** sandbox the coding tools use
    // (including ADR 0016's Unix-socket rules). `Sandbox` is cheap to clone; `None`
    // (no `shell` mounted) means `!` is unavailable.
    let bash_sandbox = sandbox.clone();
    // The MCP servers come from the user's `[mcp]` section and from `--mcp-config`
    // together (ADR 0008 §8): the file is read and validated here, before any server
    // process starts and before a session file exists, so a bad file leaves nothing
    // behind (dogfooding D2). They merge with the project `.mcp.json` below, after trust
    // is resolved (the project file needs the decision). The MCP provider itself, the
    // memory service and the coding tools are built by `radian_host::open_session`
    // (ADR 0023 U5).
    let mcp_file = match &cli.mcp_config {
        Some(path) => load_mcp_config(path)?,
        None => radian_host::McpLoad::default(),
    };
    // The TUI has no `--no-config-mcp` (only `radian exec` does, for the `explore`
    // child): the config section always loads here.
    let mcp_user = config.mcp.clone();
    // ADR 0026 D9: `--approval` and `--yes` (its shorthand) are the command line's choice.
    // When they are silent, the config's `[permissions] approval` and the journal's last
    // in-session switch decide; `radian_host::resolve_startup` owns that precedence, the
    // `full`-tier rules and the backfill record. `/new` carries the live Auto policy as an
    // explicit command-line-equivalent choice, so it behaves like `--yes` (and backfills).
    let cli_approval = if carry_auto {
        Some(radian_permissions::ApprovalPolicy::Auto)
    } else {
        match &cli.approval {
            Some(name) => Some(radian_permissions::ApprovalPolicy::parse(name).ok_or_else(
                || format!("--approval {name:?} is not one of on-ask, always, never, auto"),
            )?),
            None => cli.yes.then_some(radian_permissions::ApprovalPolicy::Auto),
        }
    };
    let config_approval = config
        .permissions
        .approval
        .as_deref()
        .and_then(radian_permissions::ApprovalPolicy::parse);
    let journal_path = dir.join(format!("{session_id}.jsonl"));
    let restored_approval = resume
        .is_some()
        .then(|| radian_host::restored_approval(&journal_path))
        .flatten();
    let startup_approval = radian_host::resolve_startup(
        cli_approval,
        config_approval,
        restored_approval,
        tier.preset(),
        resume.is_some(),
    );
    // Slice 1 of `sandbox-auto-allow-shell`: the sandbox above is already built, so the
    // profile takes the sandbox itself (`sandbox.as_ref()`) and the `workspace` rule is
    // static for the session.
    let profile = radian_tui::profile::build_profile(
        tier,
        workspace_path,
        &cli.allow_shell_command,
        sandbox.as_ref(),
    )?
    .with_approval(startup_approval.policy);

    // D4: panics after this point get a log in the diagnostics directory (ADR 0013 decision
    // 4), not next to the session: the session is the authoritative record and must not be
    // treated as a cache. Set before the terminal is entered; the hook reads it on a panic.
    let panic_log = radian_host::diagnostics_dir().map(|dir| {
        dir.join(radian_host::panic_log_name(
            session_id,
            radian_host::now_millis(),
        ))
    });
    radian_tui::terminal::set_panic_log(panic_log, Arc::clone(secrets));

    // The session facts the renderer reads, and the sink that writes them (ADR 0023 U2/U5).
    let projection = Arc::new(radian_host::SessionProjection::new());
    // The renderer reads the session facts from the host's projection, not a private copy
    // (ADR 0023 U5). Set here because the app is created before the session exists (`--pick`).
    app.set_projection(Arc::clone(&projection));
    // ADR 0043: the process control port. Bound before the session so its fan-out can
    // wrap the host's own sinks; the startup line names the path. A bind failure is a warning,
    // never a startup refusal. The bind creates a tokio listener, and this setup runs outside
    // the async `run` (sessions are opened with `runtime.block_on`), so enter the runtime first.
    let control = {
        let _guard = runtime.enter();
        match radian_host::ControlServer::bind_if(
            config.control.enabled,
            radian_host::radian_home().as_deref(),
        ) {
            Ok(server) => server,
            Err(error) => {
                startup_log.line(format!("[control] disabled: {error}"));
                None
            }
        }
    };
    if let Some(server) = &control {
        startup_log.line(format!("[control] {}", server.path().display()));
    }
    // Held for the whole session: the socket is removed on the normal return and on the panic
    // unwind, even while the driver task still holds a reference to the server (ADR 0043).
    let control_guard = control.as_ref().map(|server| server.guard());
    let live_inner: Arc<dyn EventSink> =
        Arc::new(ProjectionSink::new(tx.clone(), Arc::clone(&projection)));
    let live: Arc<dyn EventSink> = match &control {
        Some(server) => server.events(live_inner),
        None => live_inner,
    };
    // ADR 0023 U7: approvals go through the session's pending queue, so the UI shows one
    // arrival order and never supersedes a request. An `auto` policy (ADR 0026 D9) answers
    // an `ask` before any approver is consulted, so nothing reaches the queue under it.
    let approvals = radian_host::ApprovalRouting::Queued;
    // The audit sink needs the journal, which `open_session` creates; the binding is a factory
    // for that reason.
    let audit: Arc<dyn Fn(Arc<SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync> = {
        let runtime = runtime.handle().clone();
        Arc::new(move |journal: Arc<SessionJournal>| -> Arc<dyn AuditSink> {
            Arc::new(JournalAudit {
                journal,
                runtime: runtime.clone(),
            })
        })
    };
    let runtime_events_inner = {
        let tx = tx.clone();
        let secrets = Arc::clone(secrets);
        Arc::new(move |event: &radian_session::RuntimeEvent| {
            let redacted = match event {
                radian_session::RuntimeEvent::TurnFailed { turn_id, message } => {
                    let current = radian_tui::terminal::shared_secrets(&secrets);
                    radian_session::RuntimeEvent::TurnFailed {
                        turn_id: turn_id.clone(),
                        message: radian_tui::terminal::redact(message, &current),
                    }
                }
                other => other.clone(),
            };
            let _ = tx.send(UiEvent::Runtime(redacted));
        })
    };
    let runtime_events: Arc<dyn radian_runtime::RuntimeEvents> = match &control {
        Some(server) => server.runtime_events(runtime_events_inner),
        None => runtime_events_inner,
    };
    let driver_stopped: Arc<dyn Fn(radian_host::DriverEnd) + Send + Sync> = {
        let tx = tx.clone();
        let session_id = session_id.to_owned();
        Arc::new(move |end| {
            let _ = tx.send(UiEvent::DriverStopped {
                session_id: session_id.clone(),
                end,
            });
        })
    };
    let before_driver: Option<Arc<dyn Fn() + Send + Sync>> = cli
        .panic_in_driver
        .then(|| Arc::new(panic_in_driver) as Arc<dyn Fn() + Send + Sync>);
    // ADR 0021: a memory line the user must see goes into the transcript as a
    // notice instead of stderr, which the fullscreen UI would draw over. That rule covers a
    // warning too: it fires both at startup (a malformed context file, a bad skill) and
    // mid-turn (a failed memory read), and on the `--pick` path the startup ones would land on
    // the alternate screen.
    let memory_warning: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::SessionDone(vec![format!(
                "radian: [memory] {message}"
            )])));
        })
    };
    // ADR 0045: command-hook warnings ride the same UI channel as memory warnings, with
    // the `[hooks]` prefix (both fire mid-turn, when stderr would draw over the screen).
    let hooks_warning: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::SessionDone(vec![format!(
                "radian: [hooks] {message}"
            )])));
        })
    };
    // A wake-budget degrade (a background-job settle or a `Stop`-hook continuation past
    // its cap): its own input, so `update` shows one notice line per burst and marks the
    // session wake-waiting for herdr, instead of pushing a line per degrade.
    let wake_warning: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::WakeBudgetNotice(
                message.to_owned(),
            )));
        })
    };
    // The intent-stop cap (two continuations in a row ended the same way, so the third
    // hit reports `blocked` instead): its own input, so `update` shows one notice line
    // and marks the session for herdr's `intent stop` label until the next turn starts.
    let intent_stop_warning: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::IntentStopBlocked(
                message.to_owned(),
            )));
        })
    };
    // A lost job notice (every enqueue attempt failed, ADR 0037 decision 5's failure
    // rule): its own input, so `update` shows the burst's line and marks the session
    // notice-lost for herdr (`job notice lost`) until the next turn starts.
    let notice_lost_warning: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |message: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::JobNoticeLost(
                message.to_owned(),
            )));
        })
    };
    // ADR 0031 T3: resolve project trust before the session exists. The TUI is interactive, so
    // a workspace with resources and no decision asks through a startup modal (pushed after
    // the terminal is entered); the persistent write + audit happen then, once the journal
    // exists. `--trust`/`--no-trust` override for this process only.
    let cli_trust = if cli.trust {
        Some(radian_host::trust::Decision::Trust)
    } else if cli.no_trust {
        Some(radian_host::trust::Decision::Untrust)
    } else {
        None
    };
    let (trust_setup, trust_warning) =
        radian_host::trust::load_for(workspace_path, cli_trust, true);
    if let Some(warning) = &trust_warning {
        startup_log.line(format!("[trust] {warning}"));
    }
    // The status line is in the startup block either way (`trust::status_line` is pushed there
    // once the session exists): inside the alternate screen the block is where it shows.
    startup_log.print_before_screen(&radian_host::trust::status_line(&trust_setup));
    // Slice B2: the project `.mcp.json` joins the user's sources now that trust is resolved
    // — trusted (or `--trust`) loads it, untrusted starts no process. The user's own
    // precedence stands (the command line over `[mcp]`, both over the project file).
    let (project, project_lines) = radian_host::load_project(
        workspace_path,
        trust_setup.resolved.decision.as_bool(),
        false,
    );
    let merged = radian_host::merge_project_mcp(project, mcp_user, mcp_file);
    for line in project_lines.iter().chain(merged.warnings.iter()) {
        startup_log.line(format!("[mcp] {line}"));
    }
    let servers = merged.servers;
    // Slice B (hooks): the project `.radian/hooks.json` loads now that trust is resolved —
    // trusted (or `--trust`) loads it, untrusted loads nothing.
    let (project_hooks, project_hooks_lines) =
        radian_host::load_project_hooks(workspace_path, trust_setup.resolved.decision.as_bool());
    for line in &project_hooks_lines {
        startup_log.line(format!("[hooks] {line}"));
    }
    Ok((
        SessionInputs {
            sandbox,
            inherited_sandbox,
            access_builder,
            bash_sandbox,
            servers,
            startup_approval,
            profile,
            projection,
            control,
            live,
            approvals,
            audit,
            runtime_events,
            driver_stopped,
            before_driver,
            memory_warning,
            hooks_warning,
            wake_warning,
            intent_stop_warning,
            notice_lost_warning,
            trust_setup,
            project_hooks,
            memory_root,
        },
        control_guard,
    ))
}

/// `run()`'s session-opening phase (ADR 0023 U5): the one place a session is
/// established, then the control port starts serving it and the provider catalog
/// refresh is spawned. Takes the input and route structs by value (the fields the later
/// phases still need are cloned at the call site) and returns the open session.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_session_and_refresh(
    runtime: &tokio::runtime::Runtime,
    cli: &Cli,
    inputs: SessionInputs,
    route: super::run_resolve::ResolvedRoute,
    dir: &Path,
    workspace_path: &Path,
    session_id: &str,
    resume: &Option<String>,
    tier: radian_tui::profile::Tier,
    provider_name: &str,
    model_name: &str,
    provider_ref: &radian_host::ProviderRef,
    key_id: &str,
) -> Result<radian_host::LocalSession, RunError> {
    let SessionInputs {
        sandbox,
        servers,
        profile,
        projection,
        control,
        live,
        approvals,
        audit,
        runtime_events,
        driver_stopped,
        before_driver,
        memory_warning,
        hooks_warning,
        wake_warning,
        intent_stop_warning,
        notice_lost_warning,
        trust_setup,
        project_hooks,
        memory_root,
        access_builder,
        ..
    } = inputs;
    let super::run_resolve::ResolvedRoute {
        window,
        route,
        replays_reasoning,
        settings,
        config_slot,
        model,
        budget_for,
        route_price,
        ..
    } = route;
    // ADR 0043 §1: `[model.smol]` rides the shared settings into the child's launch
    // parameters; it is cloned here because `settings` moves into `LeadSetup` below.
    let smol_route = settings.smol_route.clone();
    // ADR 0023 U5: the one place a session is established; `radian-exec` calls the same function
    // with its own bindings. Every startup check that can fail has already run, so this is the
    // first thing that creates durable state. ADR 0032 slice 5: `LeadSetup::setup` fills what
    // every host-opened session shares; what the TUI decides itself is written here.
    let session = runtime
        .block_on(radian_host::open_session(
            radian_host::LeadSetup {
                settings,
                session_id: radian_host::SessionId::new(session_id.to_owned()),
                session_dir: dir.to_path_buf(),
                workspace: workspace_path.to_path_buf(),
                // The tools are mounted by default now: the workspace is the current directory
                // (or `$HOME/tmp`), not only an explicit `--workspace`.
                coding_root: Some(workspace_path.to_path_buf()),
                model,
                config_slot: std::sync::Arc::clone(&config_slot),
                budget_for: Some(budget_for),
                price_for: Some(Arc::clone(&route_price)),
                servers,
                mcp_config_path: cli.mcp_config.clone(),
                system_prompt: cli.system.clone(),
                context_budget: window
                    .budget
                    // ADR 0015: a zero budget is unset, never a real zero-token budget.
                    .filter(|tokens| *tokens > 0)
                    .map(|tokens| ContextBudget::new(tokens).with_reasoning(replays_reasoning)),
                window_context: window.context,
                sandbox,
                tier: tier.preset(),
                profile,
                access_builder: Some(access_builder),
                parent: None,
                subagent: Some(radian_host::SubagentParts {
                    executable: std::env::current_exe().ok(),
                    provider: provider_name.to_owned(),
                    model: model_name.to_owned(),
                    smol: smol_route,
                    base_url: provider_ref
                        .takes_base_url_override()
                        .then(|| route.base_url.clone()),
                    config: cli.config.clone(),
                    headers: cli.header.clone(),
                    max_tokens: cli.max_tokens,
                    thinking: cli.thinking.clone(),
                    mcp_config: cli.mcp_config.clone(),
                    trust: trust_setup.resolved.decision.as_bool(),
                }),
                tools_allow: None,
                extra_deny_read: Vec::new(),
                project_hooks,
                memory_root,
                trust: trust_setup,
                resume: resume.is_some(),
                // The effective startup route (a `/new` carry resolves here like a flag),
                // so `SessionCreated` names the route and tier this session actually runs.
                // `radian-exec` records the same `access` key (ADR 0043 1a).
                config_snapshot: serde_json::json!({
                    "provider": provider_name,
                    "model": model_name,
                    "access": tier.as_str(),
                }),
                turn_options: TurnOptions {
                    max_model_calls: Some(cli.max_model_calls),
                    ..TurnOptions::default()
                },
                turn_timeout: None,
                wording: radian_host::TUI_WORDING,
                capabilities: radian_host::SessionCapability::ALL.to_vec(),
            }
            .setup(),
            radian_host::HostBindings {
                approvals,
                shared_queue: None,
                ask_user: true,
                audit,
                events: live,
                projection: Arc::clone(&projection),
                runtime_events,
                driver_stopped,
                before_driver,
                memory_warning,
                hooks_warning,
                wake_warning,
                intent_stop_warning,
                notice_lost_warning,
            },
        ))
        .map_err(|error| error.to_string())?;
    mark("open-session-done");
    // ADR 0043: start accepting control requests for this session.
    if let Some(server) = &control {
        server.serve(runtime.handle(), radian_host::ControlTarget::of(&session));
    }
    // The providers worth refreshing now: the one this run started with. `/model`'s other
    // targets refresh on their next use (the fetch itself is on the loop's runtime).
    let refresh_targets = vec![key_id.to_owned()];
    // ADR 0026 D0: refresh the switched-to providers' model list in the background (their
    // descriptor's own URL when named, else `/v1/models`). It never blocks startup and its
    // failures are silent (the next `/model` just sees the old cache).
    runtime.spawn(async move {
        for provider_id in refresh_targets {
            let Some(key) = radian_host::auth::resolve_key(&provider_id)
                .ok()
                .flatten()
                .map(|resolved| resolved.key)
            else {
                continue;
            };
            // The retry rule (D0): skip when the cache is fresh, or when a fetch failed less
            // than the retry delay ago (the failure marker is an empty `models` write).
            let age = radian_host::provider_models_cache(&provider_id).and_then(|path| {
                std::fs::metadata(path).ok().and_then(|meta| {
                    meta.modified()
                        .ok()
                        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                })
            });
            let cached = radian_host::auth::resolve_key(&provider_id)
                .ok()
                .flatten()
                .and_then(|resolved| {
                    radian_host::read_provider_models(&provider_id, &resolved.key)
                });
            let cached_models = cached.as_ref().map_or(0, |cache| cache.models.len());
            if !radian_host::provider_models_stale(age, cached_models) {
                continue;
            }
            let base_url = radian_host::descriptor(&provider_id)
                .map(|descriptor| descriptor.default_base_url.to_owned())
                .unwrap_or_default();
            match radian_host::fetch_provider_models(&provider_id, &base_url, &key).await {
                Some(models) => {
                    let _ = radian_host::write_provider_models(&provider_id, &key, models);
                }
                None => {
                    // The failure marker: an empty list, so the retry delay applies (D0).
                    let _ = radian_host::write_provider_models(&provider_id, &key, Vec::new());
                }
            }
        }
    });
    Ok(session)
}
