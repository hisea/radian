//! The Session Facade (ADR 0023 U5): the UI's one handle on a session, and the in-process
//! implementation `open_session` builds for both binaries.
//!
//! The facade is a **door, not an owner**: the inbox, the driver, the journal, the tool
//! session and the approval routing belong to the runtime and the host. The UI holds an
//! `Arc<dyn SessionFacade>`, reads the projection through it, and sends commands; a command a
//! facade does not declare is refused, and `LocalSession` checks again when it runs one — the
//! host does not trust the UI's copy of the capability list.
//!
//! `open_session` is the one place that establishes a session: the journal, the permission and
//! memory hooks, the combined tools, the driver, the inbox and the projection. `agent-cli` and
//! `radian-tui` differ only in what they inject (their approver, audit sink, event sink,
//! runtime-event handler, wording and startup lines), which is [`HostBindings`].

mod facade;
mod ids;
mod local;
mod setup_types;

pub use facade::{DriverEnd, SessionError, SessionFacade};
pub use ids::{CommandRefused, SessionCapability, SessionCommand, SessionId, UserBashRecord};
use local::DriverTask;
pub use local::LocalSession;
pub use setup_types::{
    AccessBuilder, AccessParts, ApprovalRouting, HostBindings, SessionFacts, SessionQueue,
    SessionSetup,
};

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::{Agent, AgentConfig, Hooks, ModelUsage};
use agent_extensions::McpToolProvider;
use radian_coding::CodingTools;
use radian_permissions::{Approver, PermissionHooks, Preset};
use radian_runtime::{Driver, Inbox, ModelRoute};
use radian_sandbox::{Sandbox, SandboxSlot};
use radian_session::{ForkParent, SessionCreatedData, SessionJournal};

use crate::describe::{describe, sandbox_word};
use crate::home::{radian_home, skills_dir, user_home};
use crate::memory::{
    InjectionConfig, MemoryHooks, MemoryService, MemoryTools, ProjectState,
    injection::MemoryInjection,
};
use crate::pending::{PendingQueue, QueuedApprover, QueuedPrompter};
use crate::skills::tools::SkillTools;
use crate::skills::{self, SkillsHooks};
use crate::subagent::SubagentTools;
use crate::tools::HostTools;
use crate::trust;

/// Opens a session (ADR 0023 U5). This is the one place a session is established: the journal,
/// the hooks, the combined tools, the driver, the inbox and the projection.
///
/// Async because opening the driver is (MCP servers start and the journal is recovered); a
/// synchronous host calls it with `runtime.block_on`.
pub async fn open_session(
    setup: SessionSetup,
    bindings: HostBindings,
) -> Result<LocalSession, SessionError> {
    let t0 = std::time::Instant::now();
    let HostBindings {
        approvals,
        shared_queue,
        ask_user,
        audit,
        events,
        projection,
        runtime_events,
        driver_stopped,
        before_driver,
        memory_warning,
        memory_notice_line,
    } = bindings;
    let runtime = tokio::runtime::Handle::current();
    let SessionSetup {
        session_id,
        session_dir,
        workspace,
        coding_root,
        model,
        servers,
        system_prompt,
        context_budget,
        auto_compact,
        compaction_settings,
        compaction_settings_for,
        retries,
        sandbox,
        tier,
        profile,
        access_builder,
        parent,
        subagent: subagent_parts,
        memory_root,
        skills_extra_paths,
        skills_builtin,
        skills_disabled,
        tools_allow,
        skills_allow,
        memory_injection,
        trust,
        resume,
        config_snapshot,
        turn_options,
        turn_timeout,
        wording,
        capabilities,
        config_slot,
        config_queue,
        budget_for,
        price_for,
        shell_auto_background,
    } = setup;

    // The pending queue (ADR 0023 U7): approvals and questions share it. It exists before the
    // tools and the hooks, because both route through it.
    let queue: Arc<SessionQueue> = shared_queue.unwrap_or_else(|| Arc::new(PendingQueue::new()));
    let origin = session_id.as_str().to_string();

    // ADR 0036 C2: read the project context files once, here. They are workspace input, so
    // they never enter the journal; only the loaded paths and sizes go into the snapshot. The
    // section is appended after the host's base prompt (layer 1) and before the skills/memory
    // hooks (layer 3).
    let context = crate::context::load(&workspace, radian_home().as_deref());
    for warning in &context.warnings {
        memory_warning(warning);
    }
    let mut config_snapshot = config_snapshot;
    if !context.is_empty()
        && let Some(object) = config_snapshot.as_object_mut()
    {
        object.insert("context".to_string(), context.snapshot());
    }

    // The startup description, before the sandbox moves into the tools. The summary word
    // comes from the real backend name, not from searching the description text (which does
    // not name the backend in the `workspace` tier).
    let (profile_line, sandbox_facts) = describe(tier.sandbox_mode(), sandbox.as_ref(), wording);
    let sandbox_word = sandbox_word(
        tier.sandbox_mode(),
        sandbox.as_ref().map(Sandbox::backend_name),
    );
    // ADR 0038 slice 2: the sandbox lives in a shared slot, so `/access` can swap it and the
    // coding tools read the new one on their next call. The host keeps this clone; the tools
    // get another.
    let sandbox_slot = SandboxSlot::from_option(sandbox);
    // ADR 0039 decision 1: the jobs directory is the session's own, e.g.
    // `<sessions_dir>/<session-id>.jobs/`. It is a sibling of the session file, so the
    // sandbox's deny-read rule on the sessions directory covers it.
    let jobs_dir = session_dir.join(format!("{}.jobs", session_id.as_str()));
    // ADR 0030 O8: the coding tools (and so `shell`) are built **after** the lead registry
    // below, once the registry has installed the process heavy gate, so every session's shell
    // reads the configured gate (see the registry block).
    // ADR 0024 Q5: `ask_user` is mounted whenever a human is there to answer, **whether or not**
    // there are coding tools. It is its own provider, so a host without `--workspace` still
    // advertises it. `None` is what the model sees as "no such tool".
    let ask = ask_user.then(|| {
        // Questions are asked through this session's own pending queue.
        let prompter: Arc<dyn radian_coding::Prompter> =
            Arc::new(QueuedPrompter::new(Arc::clone(&queue), origin.clone()));
        radian_coding::AskTools::new(prompter)
    });
    let injection_config = memory_injection;
    let memory = memory_root
        .as_ref()
        .map(|root| MemoryService::open(&workspace, root));
    let memory_tools = memory.as_ref().map(|service| {
        MemoryTools::new(service.clone()).with_notice(Arc::clone(&memory_notice_line))
    });
    let memory_notice = memory_notice(&memory, &injection_config);
    radian_session::trace::took("open/memory", t0);
    let mcp = McpToolProvider::new(servers);
    let mcp_configured = mcp.configured();

    // The session file. Every check that can fail has already run in the host, so this is the
    // first thing that creates durable state.
    let journal_path = session_dir.join(format!("{}.jsonl", session_id.as_str()));
    // resume-perf B/B2: take the single-writer lock **first**, then parse under it, and reuse
    // that one parse for the journal open, the driver's recovery and the host's initial view.
    // Parsing before the lock could miss a concurrent appender's commit and truncate it away.
    let (journal, parsed, wrote_abort) = if resume {
        if !journal_path.exists() {
            return Err(SessionError::Message(format!(
                "no session {:?} in {}",
                session_id.as_str(),
                session_dir.display()
            )));
        }
        let cannot = |error: radian_session::WriteError| {
            SessionError::Message(format!(
                "cannot resume session {:?}: {error}",
                session_id.as_str()
            ))
        };
        let t_parse = std::time::Instant::now();
        let lock =
            radian_session::SessionJournal::lock_for_resume(&journal_path).map_err(cannot)?;
        let parsed = lock.parse().map_err(cannot)?;
        radian_session::trace::took("open/parse-once", t_parse);
        let (journal, wrote_abort) = lock
            .into_journal(session_id.as_str(), &parsed)
            .map_err(cannot)?;
        (Arc::new(journal), Some(parsed), wrote_abort)
    } else {
        (
            open_journal(
                &session_id,
                &session_dir,
                &workspace,
                &config_snapshot,
                parent.as_ref(),
                &journal_path,
            )?,
            None,
            false,
        )
    };
    radian_session::trace::took("open/journal", t0);
    // ADR 0027: the journal records each call's fee, computed at the
    // price in force when the call ran, so a resume does not have to re-price history with
    // whatever the catalog says later. The writer asks this closure per usage record; it
    // resolves the route from the same slot the model reads, so the journal and the live
    // footer agree (both use `cost_micros`). No `price_for`, or a catalog miss, means no fee on
    // the record — never an invented number.
    if let Some(price_for) = price_for.as_ref() {
        let slot = Arc::clone(&config_slot);
        let initial = model.current_route();
        let price_for = Arc::clone(price_for);
        journal.set_usage_fee(Arc::new(move |usage: &ModelUsage| {
            let route = ModelRoute::from_value(&slot.value()).unwrap_or_else(|| initial.clone());
            let price = price_for(&route.provider, &route.model)?;
            crate::cost::cost_micros(&price, usage)
        }));
    }
    let inbox = Inbox::durable(64, Arc::clone(&journal));

    // The built-in coding tools are built here.
    let (coding, jobs) = build_coding_tools(
        coding_root.as_deref(),
        sandbox_slot.clone(),
        &jobs_dir,
        shell_auto_background,
    )?;

    // ADR 0023 U7: a UI host routes approvals through the pending queue; the CLI's line
    // prompt answers them directly.
    let approver: Arc<dyn Approver> = match approvals {
        ApprovalRouting::Queued => {
            Arc::new(QueuedApprover::new(Arc::clone(&queue), origin.clone()))
        }
        ApprovalRouting::Direct(approver) => approver,
    };
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(approver),
        audit(Arc::clone(&journal)),
    ));
    // Skills (ADR 0025 S1): discovery is best effort — a bad skill warns and is skipped. The
    // section is appended after the memory section, so the tuple below is ((perm, memory),
    // skills) and the prompt order is basic prompt, memory, skills (ADR 0025 S6).
    let skills = {
        // The user level first, then `[skills] extra_paths` (ADR 0025 slice 4): the first
        // discovery of a name wins, so `~/.radian/skills` beats an extra root.
        let discovery = skills::discover_with_extras(
            skills_dir().as_deref(),
            &skills_extra_paths,
            skills_builtin,
            &skills_disabled,
        );
        for warning in &discovery.warnings {
            memory_warning(warning);
        }
        // An agent definition's `skills` list is the allow-list for this session's catalog.
        // `None` is every discovered skill.
        match &skills_allow {
            Some(allow) => skills::restrict_to(discovery.skills, allow),
            None => discovery.skills,
        }
    };
    let shared_skills = Arc::new(skills);
    let skills_hooks = Arc::new(SkillsHooks::new(&shared_skills));
    // ADR 0021: one injection object per session. The fixed block is computed once
    // (by whichever of the two seams runs first) and the driver's per-turn seam appends later
    // turns' new hits as one System message through the inbox's inject lane.
    let injection = MemoryInjection::new(
        memory.clone(),
        Arc::clone(&journal),
        injection_config,
        memory_warning,
    );
    let memory_hooks = Arc::new(MemoryHooks::from_shared(Arc::clone(&injection)));
    radian_session::trace::took("open/hooks+skills", t0);
    let mut tools = HostTools::new(coding, memory_tools, mcp.clone()).with_tool_allow(tools_allow);
    let ask_registered = ask.is_some();
    if let Some(ask) = ask {
        tools = tools.with_ask(ask);
    }
    if !shared_skills.is_empty() {
        tools = tools.with_skills(SkillTools::new(Arc::clone(&shared_skills)));
    }
    // ADR 0043 decision 1a: `subagent` exists in a parent session only. The depth-1 rule is
    // here, at the one place a session is established: a session whose own `parent` relation is
    // `"subagent"` never mounts the tool, so a child cannot start a grandchild.
    let is_subagent_child = parent
        .as_ref()
        .is_some_and(|parent| parent.relation() == "subagent");
    if !is_subagent_child && let Some(parts) = subagent_parts {
        tools = tools.with_subagent(SubagentTools::new(
            parts,
            session_dir.clone(),
            session_id.as_str(),
            workspace.clone(),
            coding_root.clone(),
            sandbox_slot.clone(),
            Arc::clone(&hooks),
        ));
    }
    // ADR 0036/2: the system prompt is assembled from layers. 0a <runtime> lists
    // the tools this host actually mounted and the tier/sandbox facts; 1 <role> is the role
    // prompt (or `--system`); 2 is the context files; 3 is appended by the skills/memory hooks;
    // 0b <session> is appended last by its own hook, so the static prefix stays cacheable.
    let model_name = config_snapshot
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let mounted = tools.builtin_tools();
    let workspace_mode = tier.sandbox_mode() == radian_sandbox::SandboxMode::WorkspaceWrite;
    let runtime_layer = crate::prompt::runtime_layer(&crate::prompt::Runtime {
        tools: &mounted,
        tier: tier.as_str(),
        sandbox: &sandbox_facts,
        network: workspace_mode,
        unix_sockets_restricted: workspace_mode,
    });
    let role_override = system_prompt.clone();
    let role_layer = role_override
        .clone()
        .unwrap_or_else(|| crate::prompt::DEFAULT_ROLE.to_owned());
    let system_prompt = Some(crate::prompt::assemble(
        &runtime_layer,
        &role_layer,
        &context.section,
    ));
    let role_hooks = Arc::new(crate::prompt::RoleHooks::new(
        &runtime_layer,
        &context.section,
        role_override,
    ));
    let agent_config = AgentConfig {
        system_prompt,
        context_budget,
        auto_compact,
        compaction_settings,
        retries,
        ..AgentConfig::default()
    };
    let facts_hooks = Arc::new(crate::prompt::SessionFactsHooks::new(
        workspace.clone(),
        model_name,
        Some(Arc::clone(&config_slot)),
    ));
    let agent = Arc::new(
        Agent::new(model, tools)
            .with_config(agent_config)
            .with_hooks(session_hooks(
                Arc::clone(&role_hooks),
                Arc::clone(&hooks),
                Arc::clone(&memory_hooks),
                Arc::clone(&skills_hooks),
                Arc::clone(&facts_hooks),
            )),
    );

    // MCP server **processes** start here, when the tool session opens. The session file
    // already exists, so a failure names the way to continue it.
    // A written abort changes the file on disk, so the shared parse is stale: re-parse then.
    //
    // Today an abort does not change the **fold**: an uncommitted segment at end of file and an
    // aborted one both drop the segment and leave the history and the declared calls alone
    // (`agent_core::TranscriptFold`; `an_aborted_segment_and_one_at_end_of_file_fold_identically`
    // pins that). So this re-parse is a no-op for the recovered transcript and no test can tell
    // it apart from reusing the parse. It is kept deliberately: the parse is the host's view of
    // the file, and if the abort ever gains an effect on the fold, the driver must see it.
    let recovery_parse = match (&parsed, wrote_abort) {
        (Some(parsed), false) => Some(parsed),
        _ => None,
    };
    let mut driver = Driver::open_with_slot_parsed(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        events,
        turn_options,
        config_slot.clone(),
        config_queue.clone(),
        recovery_parse,
    )
    .await
    .map_err(|error| {
        SessionError::Message(format!(
            "cannot open the session: {error}\n  the session file {} exists; after fixing the \
             problem, continue it with `--resume {}`",
            journal_path.display(),
            session_id
        ))
    })?;
    radian_session::trace::took("open/driver-recover", t0);
    driver.set_turn_timeout(turn_timeout);
    // Test hook: a controllable gap between the turn's `RunFinished` and its journaled
    // `TurnFinished` (the `/fork` commit-wait test).
    driver.set_commit_delay(
        std::env::var("RADIAN_TURN_COMMIT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_millis),
    );
    driver.set_runtime_events(runtime_events);
    // ADR 0021: the per-turn memory addition, placed in the inject lane
    // after the user's turn is claimed and before its first model call.
    driver.set_turn_additions(crate::memory::injection::addition_seam(Arc::clone(
        &injection,
    )));
    if let Some(budget_for) = budget_for.clone() {
        driver.set_budget_for(budget_for);
    }
    // ADR 0018: the same per-route resolver for the summary call's settings, so a
    // `/model` switch converts the shrunk level for the route in force.
    if let Some(settings_for) = compaction_settings_for.clone() {
        driver.set_compaction_settings_for(settings_for);
    }
    // resume-perf B: the recovered transcript goes to the host, which would otherwise open and
    // fold the same journal a second time for its initial view.
    let recovered_history = driver.take_recovered_history().unwrap_or_default();

    // ADR 0037 slices 3-4: the job notifier. The driver shares its "turn in flight" flag, so
    // the notifier can choose the `inject` lane (busy) or the waking `followup` lane (idle).
    // It writes the durable start/settle records and sends the completion System message.
    let turn_running = driver.turn_running_handle();
    let job_notifier = jobs.as_ref().map(|jobs| {
        let notifier = crate::job_notify::JobNotifier::new(
            Arc::clone(jobs),
            Arc::clone(&inbox),
            Arc::clone(&journal),
            Arc::clone(&turn_running),
        );
        notifier.spawn(&runtime);
        notifier
    });

    // ADR 0037 decision 6: a start with no settle after a crash is reported once as unknown
    // and never re-run. Injected (no wake), so the model sees it on the next turn.
    if let Some(parsed) = parsed.as_ref() {
        for (id, command) in crate::job_notify::orphan_jobs(parsed) {
            let _ = inbox
                .inject(
                    crate::job_notify::orphan_message(&id, &command),
                    agent_core::InputSource::System,
                )
                .await;
        }
    }

    // The config queue is owned by the driver; the facade only pushes to it, so clone the
    // handle before the driver moves into its task.
    let config_queue = Arc::clone(driver.config_queue());

    // The session is open, so the MCP status is known: what actually connected.
    let mcp_open = mcp.status().map(|status| (status.servers, status.tools));
    projection.set_mcp(mcp_configured, mcp_open);
    projection.set_context_window(
        context_budget
            .as_ref()
            .map(|budget| budget.input_budget_tokens),
    );
    projection.set_tier(Some(tier.as_str().to_string()));
    // F3 (ADR 0027): a resumed session folds its journal's usage records back into
    // the projection. The projection's only other writer is the live event sink, so without
    // this `--resume` showed `-` for the tokens and hid `cost` until the next response — the
    // session's earlier spending was simply gone. Both records count (a conversation request
    // and a summarizer's call, ADR 0018), and each is priced at the route that was
    // in force when it ran, not at whatever model this process starts with.
    if let Some(price_for) = price_for.as_ref()
        && let Some(parsed) = parsed.as_ref()
    {
        crate::cost::restore_usage(parsed, &projection, price_for);
    }

    // The driver runs until the host asks it to stop; dropping its `run()` future releases the
    // borrow so `shutdown()` can be called. Its end is reported by the injected callback, and
    // the result is handed back through a oneshot for the exit code.
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        if let Some(before) = before_driver {
            before();
        }
        tokio::select! {
            result = driver.run() => result,
            _ = &mut stop_rx => {
                driver.shutdown();
                driver.run().await
            }
        }
    });
    let abort = task.abort_handle();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let end = match task.await {
            Ok(Ok(())) => DriverEnd::Ok,
            Ok(Err(error)) => DriverEnd::Failed(error.to_string()),
            Err(join) => DriverEnd::Panicked(join.to_string()),
        };
        driver_stopped(end.clone());
        let _ = done_tx.send(end);
    });

    radian_session::trace::took("open/driver-task", t0);
    let facts = SessionFacts {
        profile_line,
        sandbox_facts,
        sandbox_word,
        memory_notice,
        memory_enabled: memory.is_some(),
        mcp_configured,
        mcp_open,
        // The **actual** registration, not the host's request: the startup line must not claim
        // a tool the model cannot see.
        ask_user: ask_registered,
        context_line: context.line(&workspace, user_home().as_deref()),
        context_files: context.files.clone(),
    };
    Ok(LocalSession {
        id: session_id,
        config_queue,
        config_slot,
        budget_for,
        workspace,
        journal_path,
        journal,
        inbox,
        hooks,
        sandbox_slot,
        access_builder,
        turn_running,
        projection,
        queue,
        memory,
        skills: shared_skills,
        job_notifier,
        jobs,
        trust: Mutex::new(trust),
        runtime,
        driver: DriverTask {
            stop: Mutex::new(Some(stop_tx)),
            abort,
            done: Mutex::new(Some(done_rx)),
        },
        capabilities,
        facts,
        config_snapshot,
        history: recovered_history,
    })
}

/// The agent's hook chain (ADR 0025 S6): the role hook writes layers 0a, 1
/// and 2 first, then the permission rules run, then the memory section is appended, then the
/// skills section. The order is a promise — the role layer comes first, and the sections must be
/// deterministic with memory before skills — so it lives in this one function, which
/// `open_session` calls; `tests/skills_order.rs` builds the real hooks and asserts the order on
/// the composed chain. Swapping `memory` and `skills` here is the mutation that test catches.
pub fn session_hooks(
    role: Arc<crate::prompt::RoleHooks>,
    permissions: Arc<PermissionHooks>,
    memory: Arc<MemoryHooks>,
    skills: Arc<SkillsHooks>,
    facts: Arc<crate::prompt::SessionFactsHooks>,
) -> impl Hooks {
    // The order is the promise: the role layers, permissions, memory, skills, then the volatile
    // <session> facts (ADR 0025 S6, ADR 0036 D1).
    ((((role, permissions), memory), skills), facts)
}

/// The durable audit event for one trust write (ADR 0031 T2).
fn trust_event(change: &trust::ProjectTrustChanged) -> radian_session::RuntimeEvent {
    radian_session::RuntimeEvent::trust_changed(
        change.path.to_string_lossy().into_owned(),
        change.decision.as_bool(),
    )
}

/// Creates or resumes the session file. The caller has already validated everything that can
/// fail, so this only reports the file's own state. `parent` is the derived-session lineage
/// (ADR 0022's fork or ADR 0030 O2's task agent): `Some` writes `SessionCreated.parent` with
/// its `relation`, `None` writes an ordinary session.
fn open_journal(
    session_id: &SessionId,
    dir: &Path,
    workspace: &Path,
    config_snapshot: &serde_json::Value,
    parent: Option<&ForkParent>,
    path: &Path,
) -> Result<Arc<SessionJournal>, SessionError> {
    crate::ensure_private_dir(dir).map_err(|error| {
        SessionError::Message(format!("cannot create {}: {error}", dir.display()))
    })?;
    // The snapshot is a model route reference only: never the key, a header value or a base
    // URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = match parent {
        Some(parent) => SessionCreatedData {
            workspace: workspace.to_string_lossy().into_owned(),
            config_snapshot: config_snapshot.clone(),
            parent: Some(parent.clone()),
        },
        None => SessionCreatedData::new(workspace.to_string_lossy(), config_snapshot.clone()),
    };
    match SessionJournal::create(path, session_id.as_str(), created) {
        Ok(journal) => Ok(Arc::new(journal)),
        Err(radian_session::WriteError::NotEmpty { .. }) => Err(SessionError::Message(format!(
            "session {:?} already exists in {}; continue it with `--resume {}`",
            session_id.as_str(),
            dir.display(),
            session_id
        ))),
        Err(error) => Err(SessionError::Message(format!(
            "cannot create session {:?}: {error}",
            session_id.as_str()
        ))),
    }
}

fn build_coding_tools(
    workspace: Option<&Path>,
    sandbox: SandboxSlot,
    jobs_dir: &Path,
    shell_auto_background: Option<Duration>,
) -> Result<(Option<CodingTools>, Option<Arc<radian_coding::Jobs>>), SessionError> {
    match workspace {
        Some(dir) => {
            // The slot decides whether `shell` exists: an empty slot is the `safe` tier (no
            // shell advertised), a non-empty one is a sandbox the shell can run under. The
            // tool set is fixed here at `open_session`; a later switch to a *narrower* tier
            // still denies `shell` through the permission rules (ADR 0038 slice 2).
            let tools = if sandbox.is_some() {
                CodingTools::with_sandbox_slot(dir, sandbox).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            } else {
                CodingTools::new(dir).map_err(|error| {
                    SessionError::Message(format!("--workspace {}: {error}", dir.display()))
                })?
            };
            // ADR 0037: the adoption threshold comes from `[shell]`; the host
            // resolved the default already, so `None` here means off.
            let tools = tools.with_limits(radian_coding::Limits {
                auto_background_after: shell_auto_background,
                ..radian_coding::Limits::default()
            });
            // ADR 0039 decision 1: the jobs directory is the **session's own** directory, in
            // the sessions dir the sandbox already denies reading. It is created 0700 with the
            // same helper the sessions dir uses, before the registry can write anything, and
            // passed in — the registry never derives a path from the workspace.
            crate::ensure_private_dir(jobs_dir).map_err(|error| {
                SessionError::Message(format!(
                    "cannot create the jobs directory {}: {error}",
                    jobs_dir.display()
                ))
            })?;
            // The host creates the registry so it can subscribe to job lifecycle notices and
            // write the durable start/settle records (ADR 0037).
            let jobs = Arc::new(radian_coding::Jobs::new(jobs_dir));
            Ok((Some(tools.with_jobs(Arc::clone(&jobs))), Some(jobs)))
        }
        None => Ok((None, None)),
    }
}

fn memory_notice(memory: &Option<MemoryService>, config: &InjectionConfig) -> String {
    match memory {
        Some(service) => match service.state() {
            Ok(ProjectState::Known(project)) => format!(
                "[memory] project {project}; injection on, up to {} tokens",
                config.max_tokens
            ),
            Ok(ProjectState::Unlinked { .. }) => format!(
                "[memory] no project linked; global memory only (run `radian memory project new` \
                 or `radian memory relink <ProjectId>`); injection on, up to {} tokens",
                config.max_tokens
            ),
            Err(error) => format!("[memory] unavailable: {error}"),
        },
        None => "[memory] disabled (no RADIAN_HOME or HOME)".to_owned(),
    }
}

/// The model route a **resume** would restore (ADR 0026 D8): the journal's last model
/// `ConfigChanged`. `None` when the file has none — the caller then falls back to the config
/// and, failing that, requires `--model`. Reads the session file line by line; it is the one
/// source of truth, and this runs before anything durable is created.
pub fn resumed_model_route(
    session_dir: &Path,
    session_id: &str,
) -> Option<radian_runtime::ModelRoute> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut route = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(found) = radian_runtime::ModelRoute::from_value(&value["data"]["change"])
        {
            route = Some(found);
        }
    }
    route
}

/// The permission tier a **resume** would restore (ADR 0038 decision 2): the journal's last
/// `ConfigChanged` whose change carries an `access` key. `None` when the file has none — the
/// caller then falls back to the remembered per-project tier, the config and the default.
/// Reads the session file line by line; this runs before anything durable is created.
pub fn resumed_access(session_dir: &Path, session_id: &str) -> Option<Preset> {
    let text = std::fs::read_to_string(session_dir.join(format!("{session_id}.jsonl"))).ok()?;
    let mut tier = None;
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["event"] == "ConfigChanged"
            && let Some(name) = value["data"]["change"]["access"].as_str()
            && let Some(found) = crate::access::parse_tier(name)
        {
            tier = Some(found);
        }
    }
    tier
}

#[cfg(test)]
mod tests {
    use super::*;
    use radian_sandbox::{SandboxMode, SandboxPolicy};

    fn full_access(ws: &Path) -> Sandbox {
        let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
        Sandbox::from_policy(policy).expect("full-access needs no backend")
    }

    /// ADR 0039 decision 1: the host builds the registry over the **session's own** jobs
    /// directory (0700), never over `<workspace>/.radian/jobs`, and the output file lands
    /// there at 0600.
    #[cfg(unix)]
    #[test]
    fn the_jobs_directory_is_the_sessions_own_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("radian-host-jobs-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let ws = root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let jobs_dir = root.join("sessions/sid.jobs");

        let slot = SandboxSlot::from_option(Some(full_access(&ws)));
        let (_coding, jobs) = build_coding_tools(Some(&ws), slot, &jobs_dir, None).expect("tools");
        let jobs = jobs.expect("a registry");

        assert_eq!(
            jobs.dir(),
            jobs_dir,
            "the registry uses the session jobs directory"
        );
        assert_eq!(
            std::fs::metadata(&jobs_dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the jobs directory is 0700"
        );

        let info = jobs
            .start(&full_access(&ws), &ws, "echo hi", None)
            .expect("start the job");
        let output = jobs.output_path(&info.id);
        assert!(output.starts_with(&jobs_dir), "{}", output.display());
        assert_eq!(
            std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
            0o600,
            "the output file is 0600"
        );
        assert!(
            !ws.join(".radian").join("jobs").exists(),
            "the workspace must not grow a jobs directory"
        );

        jobs.kill_all();
        let _ = std::fs::remove_dir_all(&root);
    }
}
