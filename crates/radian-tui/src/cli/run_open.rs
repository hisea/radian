//! How a run prepares the session's inputs: the sandbox, the approval policy, the event
//! sinks and the project trust (ADR 0012, ADR 0023 U2/U5/U7, ADR 0031 T3). Split from
//! `cli.rs`'s `run()` (ADR 0043 decision 5): extractions, not pure moves. No behaviour
//! change: every refusal still happens before the session file exists, sinks are wired
//! in the same order.

use super::*;

/// Everything `run()`'s input phase builds for `open_session`: the sandbox and its
/// rebuild builder, the approval policy, the control port, the event sinks and the
/// trust setup. The control socket guard is returned separately so `run()` holds it
/// for the whole session (ADR 0030 O6).
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
    pub(super) memory_notice_line: std::sync::Arc<dyn Fn(&str) + Send + Sync>,
    pub(super) trust_setup: radian_host::trust::Setup,
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
    allow_unix_sockets: &[PathBuf],
    session_id: &str,
    secrets: &radian_tui::terminal::SharedSecrets,
    startup_log: &mut StartupLog,
) -> Result<(SessionInputs, Option<radian_host::SocketGuard>), RunError> {
    // The permission tier and the sandbox. For an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 decision 2). Still before
    // the session file exists, so a refusal leaves nothing behind (dogfooding D2).
    // The tier and its source were resolved above (ADR 0038 decision 3).
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. The directory was created owner-only before the terminal was
    // entered (`ensure_private_dir` above); Linux's bwrap can only mask paths that exist.
    let memory_root = radian_host::memory_dir();
    let mut sandbox_deny_read = vec![dir.to_path_buf()];
    if let Some(memory) = &memory_root {
        sandbox_deny_read.push(memory.clone());
    }
    // ADR 0026 D4: the credential store is denied read in every enforced tier. Creating it
    // first (0700) lets Linux's bwrap mask a path that exists.
    if let Some(credentials) = radian_host::credentials_dir() {
        sandbox_deny_read.push(credentials);
    }
    // ADR 0031 T2: the trust store is denied read in every enforced tier (it is the user's map
    // of trusted directories). Creating it first (0700) lets Linux's bwrap mask it.
    if let Some(trust) = radian_host::trust_dir() {
        sandbox_deny_read.push(trust);
    }
    // ADR 0038 decision 3: the remembered-tier store is denied read like trust/ — it is the
    // user's map of which projects run unsandboxed. Creating it first lets bwrap mask it.
    if let Some(access) = radian_host::access::access_dir() {
        sandbox_deny_read.push(access);
    }
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
    // (the `safe` tier) means `!` is unavailable.
    let bash_sandbox = sandbox.clone();
    // The MCP config file is read and validated here: before any server process starts and
    // before a session file exists, so a bad file leaves nothing behind (dogfooding D2). The
    // MCP provider itself, the memory service and the coding tools are built by
    // `radian_host::open_session` (ADR 0023 U5).
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path)?,
        None => Vec::new(),
    };
    // ADR 0026 D9: `--approval` and `--yes` (its shorthand) are the command line's choice.
    // When they are silent, the config's `[permissions] approval` and the journal's last
    // in-session switch decide; `radian_host::resolve_startup` owns that precedence, the
    // `full`-tier rules and the backfill record.
    let cli_approval =
        match &cli.approval {
            Some(name) => Some(radian_permissions::ApprovalPolicy::parse(name).ok_or_else(
                || format!("--approval {name:?} is not one of on-ask, always, never, auto"),
            )?),
            None => cli.yes.then_some(radian_permissions::ApprovalPolicy::Auto),
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
    let profile =
        radian_tui::profile::build_profile(tier, workspace_path, &cli.allow_shell_command)?
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
    // ADR 0030 slice 8: the process control port. Bound before the session so its fan-out can
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
    // unwind, even while the driver task still holds a reference to the server (ADR 0030 O6).
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
        Arc::new(move |end| {
            let _ = tx.send(UiEvent::DriverStopped(end));
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
    // ADR 0021: the memory tools' notice (the project a default-scope write
    // registered) is a line the user must see, so it goes into the transcript as a notice
    // instead of stderr, which the fullscreen UI would draw over.
    let memory_notice_line: Arc<dyn Fn(&str) + Send + Sync> = {
        let tx = tx.clone();
        Arc::new(move |line: &str| {
            let _ = tx.send(UiEvent::Command(AppInput::SessionDone(vec![format!(
                "[memory] {line}"
            )])));
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
            memory_notice_line,
            trust_setup,
            memory_root,
        },
        control_guard,
    ))
}
