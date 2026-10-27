//! The session-opening path `radian exec` and `radian acp` share (ADR 0049).
//!
//! Everything here is host-neutral: the sandbox and permission profile, the trust gate, the
//! project MCP and hooks, the subagent launch parameters, and the `open_session` call with the
//! host's bindings. The caller supplies what is genuinely its own — how approvals are
//! answered, whether a human can be asked a question, its event and audit sinks, and whether
//! the trust question may be shown.
//!
//! Split from `chat.rs` so the ACP surface reuses the exact order and wording of `radian exec`
//! rather than a copy. `radian exec`'s own loop, output and signals stay in `chat`.

use std::sync::Arc;

use radian_core::{EventSink, TurnOptions};
use radian_host::SessionFacade;
use radian_permissions::AuditSink;
use tokio::sync::mpsc;

use super::approval::JournalAudit;
use super::profile;
use super::trust::trust_prompt;
use super::ui::LineHub;
use super::{Cli, Setup, SetupError};

/// What the host decides about a session before it is opened.
pub(crate) struct HostWiring {
    /// How approvals are answered: directly (the line prompt) or through the session's
    /// pending queue (the TUI and the ACP host).
    pub(crate) approvals: radian_host::ApprovalRouting,
    /// Whether `ask_user` is mounted. The ACP host has a client to ask; the line prompt does
    /// not (ADR 0024 Q5).
    pub(crate) ask_user: bool,
    /// The sink for live events.
    pub(crate) events: Arc<dyn EventSink>,
    /// Model and hook failures, for the host's output.
    pub(crate) runtime_events: Arc<dyn radian_runtime::RuntimeEvents>,
    /// Fired when the driver task ends.
    pub(crate) driver_stopped: Arc<dyn Fn(radian_host::DriverEnd) + Send + Sync>,
    /// ACP `mcpServers` (or an empty load): merged at the `--mcp-config` level.
    pub(crate) extra_mcp: radian_host::McpLoad,
    /// Whether the startup trust question may be shown. A false value takes the
    /// non-interactive untrusted default (ADR 0031 T3).
    pub(crate) trust_interactive: bool,
    /// The line hub the trust question reads; `None` when it cannot be shown.
    pub(crate) trust_lines: Option<LineHub>,
}

/// The session plus the values the line host still needs after it opens.
pub(crate) struct OpenedSession {
    pub(crate) session: Arc<radian_host::LocalSession>,
    pub(crate) audit_task: tokio::task::JoinHandle<()>,
    /// The key and every header value, for masking this host's error output.
    pub(crate) secrets: Vec<String>,
    /// The `-p` images; the ACP host has none.
    pub(crate) images: Vec<radian_coding::LoadedImage>,
}

/// Opens the session from a resolved [`Setup`] and the host's bindings.
///
/// The order is `radian exec`'s: sandbox and profile, audit channel, trust, project MCP and
/// hooks, the subagent launch parameters, `open_session`, the resume overrides, the audit drain
/// task, then the startup lines. Every failure before `open_session` leaves no session file
/// behind.
pub(crate) async fn open_host_session(
    mut setup: Setup,
    cli: &Cli,
    wiring: HostWiring,
) -> Result<OpenedSession, SetupError> {
    let HostWiring {
        approvals,
        ask_user,
        events,
        runtime_events,
        driver_stopped,
        extra_mcp,
        trust_interactive,
        trust_lines,
    } = wiring;

    let profile_arg = setup.profile;
    let workspace_path = setup.workspace.clone();
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. Create them owner-only first so Linux's bwrap (which can only mask
    // paths that exist) can hide them. The shared helper builds that list — the same list
    // the file tools deny in the `full` tier — so the two can never differ (ADR 0039
    // decision 4).
    let sessions_for_deny = super::inspect::session_dir(cli)?;
    let _ = radian_host::ensure_private_dir(&sessions_for_deny);
    // ADR 0021: the memory root is under `~/.radian/` (ADR 0020), sandbox-denied, and the
    // service is the same one `radian memory` uses. `None` only when neither `RADIAN_HOME` nor
    // `HOME` is set; then memory is simply off for this run.
    let memory_root = radian_host::memory_dir();
    let mut sandbox_deny_read = radian_host::file_tool_deny_read(&sessions_for_deny);
    // ADR 0048 decision 5: `radian arc work` adds the arc files outside the worker's
    // worktree. The same list is the file tools' deny set below (ADR 0039 decision 4).
    sandbox_deny_read.extend(cli.deny_read.iter().cloned());
    // ADR 0016: the whitelisted Unix sockets (resolved in `setup`).
    let sandbox = profile::build_sandbox(
        profile_arg,
        &workspace_path,
        &sandbox_deny_read,
        &setup.allow_unix_sockets,
        &setup.allow_env,
    )?;
    let permission_profile = profile::build_profile(
        profile_arg,
        &workspace_path,
        &cli.allow_shell_command,
        sandbox.as_ref(),
    )?
    .with_approval(setup.approval.policy);

    // The audit sink is a channel whose drain task is spawned once the session (and its
    // journal) exists.
    let (audit_tx, mut audit_rx) = mpsc::unbounded_channel();
    let audit: Arc<
        dyn Fn(Arc<radian_session::SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync,
    > = {
        let tx = audit_tx.clone();
        Arc::new(move |_journal| JournalAudit::new(tx.clone()) as Arc<dyn AuditSink>)
    };

    // ADR 0031 T3: resolve project trust before the session exists. `--trust`/`--no-trust`
    // override for this process only; a saved decision (or the non-interactive default) needs
    // no question. The persistent write + audit happen after `open_session` (they need the
    // journal), so the choice is kept.
    // The trust question requires a real terminal. Piped stdin is **not** an interactive user:
    // its first line could be a message (or even a stray `y`) and must never be read as the
    // answer to a trust question. `-p`, piped input and the ACP host are all non-interactive.
    let cli_trust = if cli.trust {
        Some(radian_host::trust::Decision::Trust)
    } else if cli.no_trust {
        Some(radian_host::trust::Decision::Untrust)
    } else {
        None
    };
    let (mut trust_setup, trust_warning) =
        radian_host::trust::load_for(&workspace_path, cli_trust, trust_interactive);
    if let Some(warning) = &trust_warning {
        eprintln!("[trust] {warning}");
    }
    let mut trust_choice = None;
    if trust_setup.needs_ask {
        // Slice B2: the question lists the project servers that trust would load.
        // Slice B (hooks): it also lists the project hooks that trust would load.
        let preview = radian_host::preview_project(&workspace_path);
        let hooks_preview = radian_host::preview_project_hooks(&workspace_path);
        if let Some(lines) = &trust_lines
            && let Some(choice) = trust_prompt(
                lines,
                &workspace_path,
                preview.as_ref(),
                hooks_preview.as_ref(),
            )
            .await
        {
            trust_setup.resolved =
                radian_host::trust::resolved_for(choice, &workspace_path).map_err(SetupError)?;
            trust_setup.needs_ask = false;
            trust_choice = Some(choice);
        }
    } else if !trust_setup.resolved.decision.as_bool()
        && trust_setup.resolved.source == radian_host::trust::Source::Default
        && radian_host::trust::has_project_resources(&workspace_path)
    {
        // The non-interactive default (ADR 0031 T3): untrusted, with the one-line way out.
        eprintln!("{}", radian_host::trust::untrusted_notice(&workspace_path));
    }
    eprintln!("{}", radian_host::trust::status_line(&trust_setup));

    // Slice B2: the project `.mcp.json` joins the user's sources now that trust is resolved
    // — trusted (or `--trust`) loads it, untrusted starts no process. The user's own
    // precedence stands (the command line over `[mcp]`, both over the project file), and
    // the `[mcp] …` lines print here, before any session file exists.
    let (project, mut project_lines) = radian_host::load_project(
        &workspace_path,
        trust_setup.resolved.decision.as_bool(),
        cli.no_config_mcp,
    );
    // The question just listed these servers: only the non-interactive default restates it.
    if trust_choice.is_some() {
        project_lines.retain(|line| !line.contains("not loaded (untrusted)"));
    }
    for line in &project_lines {
        eprintln!("[mcp] {line}");
    }
    // ACP `mcpServers` sit at the `--mcp-config` level (ADR 0049): the client's entry wins a
    // collision with the file, and both win over the config's `[mcp]` and the project file.
    let mcp_user = std::mem::take(&mut setup.mcp_user);
    let mcp_file = std::mem::take(&mut setup.mcp_file);
    let mcp_file = if extra_mcp.servers.is_empty() && extra_mcp.warnings.is_empty() {
        mcp_file
    } else {
        radian_host::merge_mcp(mcp_file, extra_mcp)
    };
    let merged = radian_host::merge_project_mcp(project, mcp_user, mcp_file);
    for warning in &merged.warnings {
        eprintln!("[mcp] {warning}");
    }
    let servers = merged.servers;

    // Slice B (hooks): the project `.radian/hooks.json` loads now that trust is resolved —
    // trusted (or `--trust`) loads it, untrusted loads nothing. The `[hooks] …` lines print
    // here, before any session file exists.
    let (project_hooks, project_hooks_lines) =
        radian_host::load_project_hooks(&workspace_path, trust_setup.resolved.decision.as_bool());
    for line in &project_hooks_lines {
        eprintln!("[hooks] {line}");
    }

    // ADR 0043 decision 2: the child `radian exec` this session may start. The model, the
    // provider and the (generic-provider) endpoint are the parent's resolved ones; the key is
    // not passed — the child reads the same `auth.json`. `--config` is forwarded only when it
    // was explicit, and a child is told to trust the workspace only when the parent does.
    let subagent_parts = radian_host::SubagentParts {
        executable: std::env::current_exe().ok(),
        provider: setup.provider_id.clone(),
        model: setup.model_name.clone(),
        smol: setup.settings.smol_route.clone(),
        base_url: setup.base_url.clone(),
        config: cli.config.clone(),
        headers: setup.headers.clone(),
        max_tokens: cli.max_tokens,
        thinking: cli.thinking.clone(),
        mcp_config: cli.mcp_config.clone(),
        trust: trust_setup.resolved.decision.as_bool(),
    };
    // ADR 0043 decision 2 point 1 and 6: `--parent` is the lineage record; the seq is 0
    // because the tool runs inside a turn, not at a turn boundary.
    let parent = cli
        .parent
        .as_ref()
        .map(|id| radian_session::ForkParent::subagent(id.clone(), cli.parent_seq));
    // The hidden `--tools` list is a physical unmount (the `explore` child's three read tools).
    let tools_allow = cli.tools.as_deref().map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect::<Vec<String>>()
    });

    // ADR 0023 U5: the one place a session is established; `radian-tui` calls the same function
    // with its own bindings. ADR 0032 slice 5: `LeadSetup::setup` fills what every host-opened
    // session shares; what this host decides itself is written here.
    let auto_compact = setup.settings.auto_compact;
    let resume_session = cli.resume.is_some();
    // Values the line host still needs after this function, taken before `setup` is consumed.
    let secrets = setup.secrets.clone();
    let images = std::mem::take(&mut setup.images);
    let session_id = setup.session_id.clone();
    let context_budget = setup.context_budget;
    let model_name = setup.model_name.clone();
    let session = radian_host::open_session(
        radian_host::LeadSetup {
            settings: setup.settings,
            session_id: radian_host::SessionId::new(setup.session_id.clone()),
            session_dir: sessions_for_deny.clone(),
            workspace: workspace_path.clone(),
            coding_root: cli.workspace.clone().map(std::path::PathBuf::from),
            model: setup.model,
            config_slot: setup.config_slot,
            budget_for: Some(setup.budget_for),
            price_for: setup.price_for,
            servers,
            mcp_config_path: setup.mcp_config.clone(),
            system_prompt: setup.system_prompt.clone(),
            context_budget,
            window_context: setup.window.context,
            sandbox,
            tier: profile_arg.preset(),
            profile: permission_profile,
            access_builder: None,
            parent,
            subagent: Some(subagent_parts),
            tools_allow,
            extra_deny_read: cli.deny_read.clone(),
            project_hooks,
            memory_root,
            trust: trust_setup,
            resume: cli.resume.is_some(),
            config_snapshot: serde_json::json!({
                "provider": setup.provider.as_str(),
                "model": model_name,
                // ADR 0043 1a: the tier the session started in. A child records it here, so
                // `SessionCreated` answers "which tier did the parent give me?" without asking
                // the process (and a test can read it after the child exits).
                "access": setup.profile.as_str(),
            }),
            turn_options: TurnOptions {
                max_model_calls: Some(setup.max_model_calls),
                deadline: None,
                // The hidden, test-only `--tool-cancel-grace-ms` overrides the 5 s default.
                tool_cancel_grace: cli.tool_cancel_grace_ms.map_or(
                    radian_core::DEFAULT_TOOL_CANCEL_GRACE,
                    std::time::Duration::from_millis,
                ),
                ..TurnOptions::default()
            },
            turn_timeout: setup.turn_timeout_secs.map(std::time::Duration::from_secs),
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        }
        .setup(),
        radian_host::HostBindings {
            approvals,
            shared_queue: None,
            ask_user,
            audit,
            events,
            projection: Arc::new(radian_host::SessionProjection::new()),
            runtime_events,
            driver_stopped,
            before_driver: None,
            memory_warning: Arc::new(|message: &str| eprintln!("[memory] {message}")),
            hooks_warning: Arc::new(|message: &str| eprintln!("[hooks] {message}")),
            wake_warning: Arc::new(|message: &str| eprintln!("[wake] {message}")),
            notice_lost_warning: Arc::new(|message: &str| eprintln!("[lost] {message}")),
            intent_stop_warning: Arc::new(|message: &str| eprintln!("[intent-stop] {message}")),
        },
    )
    .await
    .map_err(|error| SetupError(error.to_string()))?;

    // ADR 0031 T3: persist the choice now that the journal exists, so the write audits once.
    if let Some(choice) = trust_choice
        && let Err(error) = session.record_trust_async(choice).await
    {
        eprintln!("radian: cannot write trust.json: {error}");
    }

    // ADR 0026 D8: an explicit `--model`/`--provider` wins over the resumed change, and the
    // override is recorded so the journal stays the authority for the next resume.
    if resume_session && let Some((recorded_provider, recorded_model)) = session.model_route() {
        let model_differs = cli.model.as_deref().is_some_and(|m| m != recorded_model);
        let provider_differs = cli
            .provider
            .as_deref()
            .is_some_and(|p| p != recorded_provider);
        if model_differs || provider_differs {
            eprintln!(
                "[model] resume recorded {recorded_provider}/{recorded_model}; using the \
                 command line's {}/{}",
                setup.provider_id, setup.model_name
            );
            session.queue_config(
                radian_host::ModelRoute::new(&setup.provider_id, &setup.model_name).to_value(),
            );
        }
    }
    // ADR 0026 D9: an explicit command-line choice is recorded, so a resume restores it. The
    // hooks already carry the policy (the profile above), so this is the record only — no
    // audit line: nothing switched inside the session. The driver journals it at the next
    // turn boundary.
    if let Some(policy) = setup.approval.backfill {
        session
            .config_queue()
            .push(serde_json::json!({ "approval": policy.as_str() }));
    }
    // ADR 0038 decision 2 (same rule as /model D8): an explicit `--profile` on a resume wins
    // over the journal's recorded access, and the override is recorded so the journal stays
    // the authority for the next resume.
    let recorded_access = super::inspect::session_dir(cli)
        .ok()
        .and_then(|dir| radian_host::resumed_access(&dir, &session_id));
    if resume_session
        && cli.profile.is_some()
        && recorded_access != cli.profile.map(|explicit| explicit.preset())
    {
        session
            .config_queue()
            .push(serde_json::json!({ "access": setup.profile.as_str() }));
    }

    // The audit drain task: every decision goes into the channel and is appended here. It is
    // flushed by dropping the senders and awaiting the task before the process exits.
    let audit_journal = Arc::clone(session.journal());
    let audit_task = tokio::spawn(async move {
        while let Some(event) = audit_rx.recv().await {
            let _ = audit_journal.append_runtime_event(event).await;
        }
    });

    // The startup lines, in the order the line host has always printed them. The facts come
    // from the session; the wording is the host's (ADR 0012).
    eprintln!("[profile] {}", session.facts().profile_line);
    eprintln!("[sandbox] {}", session.facts().sandbox_facts);
    // ADR 0036: the loaded context files (paths and sizes), or `none`.
    eprintln!("[context] {}", session.facts().context_line);
    // ADR 0026 D9: the startup line names the auto policy whichever way it was chosen, and
    // the warnings say when a config value was ignored or a restore refused.
    if setup.approval.policy == radian_permissions::ApprovalPolicy::Auto {
        eprintln!(
            "[approval] auto: tool calls are approved without asking; deny rules and the \
             sandbox still apply"
        );
    }
    for warning in &setup.approval.warnings {
        eprintln!("[approval] {warning}");
    }
    // ADR 0008: which file was read and what was ignored (never a value).
    match &setup.config_path {
        Some(path) => eprintln!("[config] {}", path.display()),
        None => eprintln!("[config] none"),
    }
    for key in &setup.config_skipped {
        eprintln!("[config] ignored key `{key}`");
    }
    // The numbers, the effective input budget B, and where they came from (ADR 0018).
    let compaction = if auto_compact {
        "automatic compaction on"
    } else {
        "automatic compaction off"
    };
    match &context_budget {
        Some(budget) => eprintln!(
            "[budget] {}; a provider-reported input is trusted as reported and only the growth \
             after it is estimated ({} ASCII bytes per token, at least 1 token per non-ASCII \
             character); {compaction}",
            setup.window.describe(),
            budget.bytes_per_token
        ),
        None => eprintln!("[budget] {}; {compaction}", setup.window.describe()),
    }
    eprintln!("{}", session.facts().memory_notice);
    // ADR 0024 Q5: the line host has no interactive user, so `ask_user` is not registered
    // there; a host with a client (the ACP surface) registers it.
    eprintln!("{}", session.facts().ask_user_line());

    Ok(OpenedSession {
        session: Arc::new(session),
        audit_task,
        secrets,
        images,
    })
}
