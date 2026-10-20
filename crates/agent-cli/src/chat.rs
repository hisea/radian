//! The chat session (`chat`, ADR 0023 U5): establish the session through
//! `radian_host::open_session`, print the startup lines, and drive the prompt loop.
//!
//! Split from `lib.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use agent_core::{EventSink, TurnOptions};
use radian_host::SessionFacade;
use radian_permissions::{Approver, AuditSink};
use tokio::sync::{Notify, mpsc};

use super::approval::{JournalAudit, TerminalApprover};
use super::events::spawn_signals;
use super::jsonl;
use super::profile;
use super::redact::redact;
use super::trust::trust_prompt;
use super::turn::{LoopDone, Run, interactive_loop, one_shot};
use super::ui::{LineHub, Ui};
use super::{Cli, Setup, SetupError};

pub(crate) async fn chat(setup: Setup, mut cli: Cli) -> Result<i32, SetupError> {
    let interactive = cli.prompt.is_none();
    // `-p -` reads the prompt from stdin to EOF before anything else starts: the line hub
    // below takes stdin as soon as it spawns, so reading here first is the only way the
    // prompt gets the whole pipe.
    if cli.prompt.as_deref() == Some("-") {
        let prompt = tokio::task::spawn_blocking(|| jsonl::read_prompt(std::io::stdin()))
            .await
            .map_err(|error| SetupError(format!("cannot read the prompt from stdin: {error}")))?
            .map_err(|error| SetupError(format!("cannot read the prompt from stdin: {error}")))?;
        cli.prompt = Some(prompt);
    }
    let jsonl_mode = cli.output == Some(super::jsonl::OutputMode::Jsonl);
    // The `--output jsonl` stdout pipeline: durable journal bytes and live delta lines meet
    // on one channel and one task owns stdout, so a blocked pipe never stalls a journal
    // write. Created before the session so the UI can hold the live sender from the start.
    let (jsonl_tx, jsonl_rx) = if jsonl_mode {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };
    let lines = LineHub::spawn();
    // One-shot mode with no terminal on stdin has nobody to ask; interactive mode reads its
    // answers from the same line hub as the prompt loop.
    let can_ask = interactive || std::io::stdin().is_terminal();

    // The profile and the sandbox: what may run, and what the OS enforces. `full` is only
    // reachable because the user named it; a failed probe is an error, never a fallback. All
    // of this runs **before** the session file is created (dogfooding D2).
    //
    // Slice 1 of `sandbox-auto-allow-shell`: the sandbox is built first and the profile
    // after it, so the `workspace` rule is static for the session — `allow` inside the
    // sandbox (`sandbox.is_some()`: a probed backend, or `inherited`), `ask` without
    // one. Approval still cannot widen the sandbox (ADR 0012 P1).
    let profile_arg = setup.profile;
    let workspace_path = setup.workspace.clone();
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. Create them owner-only first so Linux's bwrap (which can only mask
    // paths that exist) can hide them.
    let sessions_for_deny = super::inspect::session_dir(&cli)?;
    let _ = radian_host::ensure_private_dir(&sessions_for_deny);
    // ADR 0021: the memory root is under `~/.radian/` (ADR 0020), sandbox-denied, and the
    // service is the same one `radian memory` uses. `None` only when neither `RADIAN_HOME` nor
    // `HOME` is set; then memory is simply off for this run.
    let memory_root = radian_host::memory_dir();
    let mut sandbox_deny_read = vec![sessions_for_deny.clone()];
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
        sandbox.is_some(),
    )?
    .with_approval(setup.approval.policy);

    // The approver and the audit sink. An `auto` policy (ADR 0026 D9, from `--yes`/`--approval
    // auto`/config/resume) answers every `ask` before any approver is consulted, so the
    // terminal approver only runs for a genuine `on-ask` session. The audit sink is a channel
    // whose drain task is spawned once the session (and its journal) exists.
    let approver: Arc<dyn Approver> = Arc::new(TerminalApprover {
        lines: lines.clone(),
        can_ask,
    });
    let (audit_tx, mut audit_rx) = mpsc::unbounded_channel();
    let audit: Arc<
        dyn Fn(Arc<radian_session::SessionJournal>) -> Arc<dyn AuditSink> + Send + Sync,
    > = {
        let tx = audit_tx.clone();
        Arc::new(move |_journal| JournalAudit::new(tx.clone()) as Arc<dyn AuditSink>)
    };
    let ui = Arc::new(Ui::new(setup.max_model_calls, jsonl_tx.clone()));
    let projection = Arc::new(radian_host::SessionProjection::new());
    let signals = spawn_signals();
    let driver_ended = Arc::new(Notify::new());
    let runtime_events_inner = {
        let secrets = Arc::new(setup.secrets.clone());
        Arc::new(move |event: &radian_session::RuntimeEvent| {
            if let radian_session::RuntimeEvent::TurnFailed { message, .. } = event {
                let message = redact(message, &secrets);
                eprintln!("radian: {message}");
                // ADR 0018's fallback: a recognizable context overflow gets one extra line.
                if radian_host::is_context_overflow(&message) {
                    eprintln!("radian: {}", radian_host::CONTEXT_OVERFLOW_HELP);
                }
            }
        })
    };

    // ADR 0043: the process control port. Bound before the session so its fan-out can
    // wrap the host's own sinks; a bind failure is a warning, never a startup refusal, so a
    // session always opens. The startup line names the path (a client finds it by pid too).
    let control = match radian_host::ControlServer::bind_if(
        setup.control_enabled,
        radian_host::radian_home().as_deref(),
    ) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("[control] disabled: {error}");
            None
        }
    };
    if let Some(server) = &control {
        eprintln!("[control] {}", server.path().display());
    }
    // Held for the whole session: the socket is removed on the normal return and on a panic
    // unwind, even while the driver task still holds a reference to the server (ADR 0043).
    let _control_guard = control.as_ref().map(|server| server.guard());
    let events: Arc<dyn EventSink> = match &control {
        Some(server) => server.events(Arc::clone(&ui) as Arc<dyn EventSink>),
        None => Arc::clone(&ui) as Arc<dyn EventSink>,
    };
    let runtime_events: Arc<dyn radian_runtime::RuntimeEvents> = match &control {
        Some(server) => server.runtime_events(runtime_events_inner),
        None => runtime_events_inner,
    };

    // ADR 0031 T3: resolve project trust before the session exists. `--trust`/`--no-trust`
    // override for this process only; a saved decision (or the non-interactive default) needs
    // no question. The persistent write + audit happen after `open_session` (they need the
    // journal), so the choice is kept.
    // The trust question requires a real terminal. Piped stdin is **not** an interactive user:
    // its first line could be a message (or even a stray `y`) and must never be read as the
    // answer to a trust question. `-p` and piped input are both non-interactive here, so they
    // take the untrusted default and the one-line hint. (The other CLI prompts keep their
    // existing `can_ask` rule; only trust is this strict.)
    let trust_interactive = cli.prompt.is_none() && std::io::stdin().is_terminal();
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
        if let Some(choice) = trust_prompt(&lines, &workspace_path).await {
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

    // ADR 0043 decision 1a: the child `radian exec` this session may start. The model, the
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
    // ADR 0043 decision 1a point 1 and 6: `--parent` is the lineage record; the seq is 0
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
    // The budget startup line below reads this after the settings are handed over.
    let auto_compact = setup.settings.auto_compact;
    let resume_session = cli.resume.is_some();
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
            servers: setup.servers,
            system_prompt: setup.system_prompt.clone(),
            context_budget: setup.context_budget,
            sandbox,
            tier: profile_arg.preset(),
            profile: permission_profile,
            access_builder: None,
            parent,
            subagent: Some(subagent_parts),
            tools_allow,
            memory_root,
            trust: trust_setup,
            resume: cli.resume.is_some(),
            config_snapshot: serde_json::json!({
                "provider": setup.provider.as_str(),
                "model": setup.model_name,
                // ADR 0043 1a: the tier the session started in. A child records it here, so
                // `SessionCreated` answers "which tier did the parent give me?" without asking
                // the process (and a test can read it after the child exits).
                "access": setup.profile.as_str(),
            }),
            turn_options: TurnOptions {
                max_model_calls: Some(setup.max_model_calls),
                deadline: None,
                ..TurnOptions::default()
            },
            turn_timeout: setup.turn_timeout_secs.map(Duration::from_secs),
            wording: radian_host::CLI_WORDING,
            capabilities: radian_host::SessionCapability::ALL.to_vec(),
        }
        .setup(),
        radian_host::HostBindings {
            approvals: radian_host::ApprovalRouting::Direct(approver),
            shared_queue: None,
            ask_user: false,
            audit,
            events,
            projection: Arc::clone(&projection),
            runtime_events,
            driver_stopped: {
                let ended = Arc::clone(&driver_ended);
                Arc::new(move |_| ended.notify_one())
            },
            before_driver: None,
            memory_warning: Arc::new(|message: &str| eprintln!("[memory] {message}")),
            hooks_warning: Arc::new(|message: &str| eprintln!("[hooks] {message}")),
            wake_warning: Arc::new(|message: &str| eprintln!("[wake] {message}")),
        },
    )
    .await
    .map_err(|error| SetupError(error.to_string()))?;
    let session = Arc::new(session);
    // ADR 0043: start accepting control requests for this session.
    if let Some(server) = &control {
        server.serve(
            &tokio::runtime::Handle::current(),
            radian_host::ControlTarget::of(&session),
        );
    }

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
    let recorded_access = super::inspect::session_dir(&cli)
        .ok()
        .and_then(|dir| radian_host::resumed_access(&dir, &setup.session_id));
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

    // The startup lines, in the order this host has always printed them. The facts come from
    // the session; the wording is the host's (ADR 0012 decision 4).
    eprintln!("[profile] {}", session.facts().profile_line);
    eprintln!("[sandbox] {}", session.facts().sandbox_facts);
    // ADR 0036 C2: the loaded context files (paths and sizes), or `none`.
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
    match &setup.context_budget {
        Some(budget) => eprintln!(
            "[budget] {}; a provider-reported input is trusted as reported and only the growth \
             after it is estimated ({} ASCII bytes per token, at least 1 token per non-ASCII \
             character); {compaction}",
            setup.window.describe(),
            budget.bytes_per_token
        ),
        None => eprintln!(
            "[budget] {} (ADR 0018); {compaction}",
            setup.window.describe()
        ),
    }
    eprintln!("{}", session.facts().memory_notice);
    // ADR 0024 Q5: this host has no interactive user, so `ask_user` is not registered.
    eprintln!("{}", session.facts().ask_user_line());

    // The approval prompt and the prompt loop share stdin. While approval may read, the loop
    // must not compete for a line; with `--yes` (or nobody to ask) it may.
    ui.set_approval_may_read(can_ask && !cli.yes);
    // `--output jsonl`, installed once the journal exists: the tee returns the prefix length,
    // the prefix is read from the file (the lines written before the tee, e.g.
    // `SessionCreated`), and the printer owns stdout from here on. Because install and every
    // write take the same lock, no line is missed or printed twice.
    let jsonl_printer = if jsonl_mode {
        let tx = jsonl_tx
            .clone()
            .expect("the jsonl channel exists in jsonl mode");
        let prefix_len = session.journal().set_tee(tx) as usize;
        let bytes = std::fs::read(session.journal_path()).map_err(|error| {
            SetupError(format!(
                "cannot read the session file {}: {error}",
                session.journal_path().display()
            ))
        })?;
        let end = prefix_len.min(bytes.len());
        let prefix = bytes[..end]
            .split_inclusive(|byte: &u8| *byte == b'\n')
            .map(|line| line.to_vec())
            .collect::<Vec<_>>();
        Some(jsonl::spawn_printer(
            prefix,
            jsonl_rx.expect("the jsonl channel exists in jsonl mode"),
        ))
    } else {
        None
    };
    // What the `Result` record reports. Read before the drops below: the projection lives
    // with the session.
    let jsonl_session_id = session.id().as_str().to_owned();
    let jsonl_journal_path = session.journal_path().to_path_buf();
    let jsonl_cost_estimated = session.projection().read().cost_estimated();
    let mut run = Run {
        inbox: Arc::clone(session.inbox()),
        hooks: Arc::clone(session.hooks()),
        ui: Arc::clone(&ui),
        driver_ended,
        signals,
        session: Arc::clone(&session),
        secrets: setup.secrets.clone(),
    };
    // Count started-but-unresolved turns for `exec -p`'s exit decision (see
    // `CommandHooks::note_turn_started`): every turn start raises it synchronously,
    // `on_turn_end` resolves it, so the run never decides in the gap between
    // `RunFinished` emission and the Stop verdict.
    let command_hooks = run.session.command_hooks();
    run.ui.set_turn_started_hook(Arc::new(move || {
        command_hooks.note_turn_started();
    }));
    let done: LoopDone = if let Some(prompt) = cli.prompt {
        one_shot(&mut run, prompt).await
    } else {
        interactive_loop(&mut run, &lines).await
    };
    ui.end_of_turn();
    // Flush the audit: dropping the session drops the agent and the hooks, and dropping the
    // last sender closes the channel; the task writes what is left before it ends. The UI is
    // dropped here too: in `--output jsonl` mode it holds a live-line sender, and the
    // printer below only ends once every sender is gone.
    drop(run);
    drop(ui);
    drop(session);
    drop(audit_tx);
    let _ = audit_task.await;
    // The `Result` line goes after the last durable line: every tee sender is dropped by now
    // (the UI with its run, the journal with its session and the flushed audit task), so
    // awaiting the printer drains the whole stream before the summary is folded.
    if let Some(printer) = jsonl_printer {
        drop(jsonl_tx);
        let _ = printer.await;
        let summary = jsonl::summarize_journal(&jsonl_journal_path);
        let line = jsonl::result_line(&jsonl_session_id, &done.end, &summary, jsonl_cost_estimated);
        let _ = std::io::Write::write_all(&mut std::io::stdout().lock(), &line);
    }
    Ok(done.code)
}
