//! Opens a session (ADR 0023 U5): the one place a session is established — the
//! journal, the hooks, the combined tools, the driver, the inbox and the projection.
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::sync::{Arc, Mutex};

use agent_extensions::McpToolProvider;
use radian_core::{Agent, AgentConfig, ModelUsage};
use radian_permissions::{Approver, PermissionHooks};
use radian_runtime::{Driver, Inbox, ModelRoute};
use radian_sandbox::{Sandbox, SandboxSlot};

use super::facade::{DriverEnd, SessionError};
use super::local::{DriverTask, LocalSession};
use super::setup_types::{ApprovalRouting, HostBindings, SessionFacts, SessionQueue, SessionSetup};
use super::support::{build_coding_tools, memory_notice, open_journal, session_hooks};
use crate::describe::{describe, sandbox_word};
use crate::home::{agents_skills_dir, radian_home, skills_dir, user_home};
use crate::memory::{
    MemoryHooks, MemoryInjection, MemoryService, MemoryTools, ScopeSelection, exceeds_budget,
};
use crate::pending::{PendingQueue, QueuedApprover, QueuedPrompter};
use crate::skills::tools::SkillTools;
use crate::skills::{self, SkillsHooks};
use crate::subagent::SubagentTools;
use crate::tools::HostTools;

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
        hooks_warning,
        wake_warning,
        notice_lost_warning,
        intent_stop_warning,
    } = bindings;
    let runtime = tokio::runtime::Handle::current();
    let SessionSetup {
        session_id,
        session_dir,
        workspace,
        coding_root,
        model,
        servers,
        mcp_config_path,
        system_prompt,
        context_budget,
        window_context,
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
        hooks: hooks_config,
        project_hooks: project_hooks_config,
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
    } = setup;

    // The pending queue (ADR 0023 U7): approvals and questions share it. It exists before the
    // tools and the hooks, because both route through it.
    let queue: Arc<SessionQueue> = shared_queue.unwrap_or_else(|| Arc::new(PendingQueue::new()));
    let origin = session_id.as_str().to_string();

    // ADR 0036: read the project context files once, here. They are workspace input, so
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
    // The `<runtime>` Unix-socket sentence follows the backend's effective answer (slice 0:
    // Linux bwrap denies every Unix socket with seccomp; macOS seatbelt denies with a
    // whitelist), the way
    // `git_writable_except_exec` already asks the backend (ADR 0038 slice ④) — never the
    // tier alone, which would claim a deny the backend does not enforce.
    let unix_sockets_restricted = sandbox
        .as_ref()
        .is_some_and(|sandbox| sandbox.restricts_unix_sockets());
    // The prompt's own tier facts (layer 0a): STE sentences built from the tier and the
    // backend's effective answers — never the TUI's `describe` string. `inherited`
    // claims no local rules at all (ADR 0016 decision 4).
    let tier_facts = match (tier.sandbox_mode(), sandbox.as_ref()) {
        (radian_sandbox::SandboxMode::FullAccess, _) => crate::prompt::SandboxFacts::None,
        (radian_sandbox::SandboxMode::WorkspaceWrite, Some(sandbox))
            if sandbox.backend_name() == "inherited" =>
        {
            crate::prompt::SandboxFacts::Inherited
        }
        (radian_sandbox::SandboxMode::WorkspaceWrite, Some(sandbox)) => {
            crate::prompt::SandboxFacts::Workspace {
                git_writable: sandbox.git_writable_except_exec(),
            }
        }
        (radian_sandbox::SandboxMode::WorkspaceWrite, None) => {
            crate::prompt::SandboxFacts::Workspace {
                git_writable: false,
            }
        }
    };
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
    // Session setup order: the coding tools (and so `shell`) are built here, before the MCP
    // provider, so a failure in either does not leave half a session behind.
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
    // ADR 0021 D7: `memory_search` mounts only when the Active records' estimated total
    // exceeds the injection budget — what fits in the fixed block needs no search. The view
    // is the fixed block's (Project when linked, then Global); the decision is once per
    // session. A read failure warns and mounts nothing (invariant 2), never failing the open.
    let memory_tools =
        memory.as_ref().and_then(
            |service| match service.list(ScopeSelection::Default, false) {
                Ok(view) if exceeds_budget(&view, &injection_config) => {
                    Some(MemoryTools::new(service.clone()))
                }
                Ok(_) => None,
                Err(error) => {
                    memory_warning(&format!(
                        "memory read failed; `memory_search` is not mounted: {error}"
                    ));
                    None
                }
            },
        );
    let memory_notice = memory_notice(&memory, &injection_config);
    radian_session::trace::took("open/memory", t0);
    let mcp_server_configs = servers.clone();
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

    // Skills (ADR 0025 S1): discovery is best effort — a bad skill warns and is skipped. It
    // runs before the coding tools are built so their read-only roots include each
    // discovered skill's directory (ADR 0039 decision 4): the same set discovery loads, so an
    // untrusted project's skills are never roots (ADR 0031 T1). The section is appended
    // after the memory section, so the tuple below is ((perm, memory), skills) and the
    // prompt order is basic prompt, memory, skills (ADR 0025 S6).
    let skills = {
        // The two user levels first (`$RADIAN_HOME/skills`, then the Agent Skills standard
        // `$HOME/.agents/skills`), then `[skills] extra_paths`, then the trusted project's
        // `.agents/skills` from the workspace up to the repository root (ADR 0025 S1): the
        // first discovery of a name wins, so `$RADIAN_HOME/skills` beats `~/.agents/skills`
        // and a user-level skill beats a project one. An untrusted project's roots are empty,
        // so its skills are never read (ADR 0031 T1).
        let discovery = skills::discover_with_project(
            skills_dir().as_deref(),
            agents_skills_dir().as_deref(),
            &skills_extra_paths,
            &skills::project_roots_for(&workspace, trust.resolved.decision.as_bool()),
            skills_builtin,
            &skills_disabled,
        );
        for warning in &discovery.warnings {
            memory_warning(warning);
        }
        // The session's skill allow-list keeps only the named skills in this session's catalog.
        // `None` is every discovered skill.
        match &skills_allow {
            Some(allow) => skills::restrict_to(discovery.skills, allow),
            None => discovery.skills,
        }
    };
    let shared_skills = Arc::new(skills);

    // The built-in coding tools are built here. `harden_git` is the trust decision
    // (ADR 0039 decision 5): an untrusted project hardens git in every shell child.
    // Skill roots are the discovered filesystem skills' directories (built-ins are
    // embedded and have no directory); the deny set is the host's deny-read list, the same
    // source the sandbox policy uses (ADR 0039 decision 4). The tools also hold the shared
    // sandbox slot as their tier source, so a tier switch changes their reads at the next
    // call (ADR 0038).
    let harden_git = !trust.resolved.decision.as_bool();
    let skill_dirs: Vec<std::path::PathBuf> = shared_skills
        .iter()
        .filter(|skill| skill.body.is_none())
        .map(|skill| skill.dir.clone())
        .collect();
    let (coding, jobs) = build_coding_tools(
        coding_root.as_deref(),
        sandbox_slot.clone(),
        &jobs_dir,
        harden_git,
        &skill_dirs,
        &crate::home::file_tool_deny_read(&session_dir),
    )?;
    // ADR 0037 §6: a resumed session clears its jobs directory. The registry is built empty
    // above, so at this point no file in the directory can belong to it and nothing is
    // running (the resume path holds the session's exclusive writer lock, so no other live
    // process can own these files either). Only the resume path sweeps; a failure warns
    // once and never fails the open.
    if resume && let Some(jobs) = jobs.as_ref() {
        let summary = jobs.sweep_unregistered();
        if let Some(error) = summary.first_error.as_deref() {
            memory_warning(&format!(
                "could not sweep the jobs directory {}: {error}",
                jobs_dir.display()
            ));
        }
    }

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
    // ADR 0045 slices A+B: the user-level command hooks layered over the project hooks,
    // chained directly after the permissions (a hook can only narrow what the rules
    // already allowed). The inbox carries the Stop continuations; the runtime spawns them
    // off the sync turn-end hook.
    let command_hooks = Arc::new(crate::CommandHooks::new(
        &hooks_config,
        &project_hooks_config,
        &workspace,
        session_id.as_str(),
        sandbox_slot.clone(),
        &inbox,
        &runtime,
        hooks_warning,
        Arc::clone(&wake_warning),
        intent_stop_warning,
    ));
    // Skills (ADR 0025 S1): discovery is best effort — a bad skill warns and is skipped. The
    // section is appended after the memory section, so the tuple below is ((perm, memory),
    // skills) and the prompt order is basic prompt, memory, skills (ADR 0025 S6).
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
    let spec_sink: std::sync::Arc<std::sync::Mutex<Option<Vec<radian_core::ToolSpec>>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let mut tools = HostTools::new(coding, memory_tools, mcp.clone())
        .with_tool_allow(tools_allow)
        .with_spec_sink(std::sync::Arc::clone(&spec_sink));
    // ADR 0047: the persistent goal. Folded once from the journal at open (a resume parks
    // an active goal as paused); the `goal` tool, the prompt hook and the TUI share the
    // result, and every change is journaled first so the fold always rebuilds it.
    let goal_share: crate::goal::SharedGoal =
        Arc::new(Mutex::new(parsed.as_ref().and_then(|parsed| {
            radian_session::fold_for_resume(parsed.events())
        })));
    tools = tools.with_goal(crate::goal::GoalTools::new(
        Arc::clone(&goal_share),
        Arc::clone(&journal),
    ));
    let ask_registered = ask.is_some();
    if let Some(ask) = ask {
        tools = tools.with_ask(ask);
    }
    if !shared_skills.is_empty() {
        tools = tools.with_skills(SkillTools::new(Arc::clone(&shared_skills)));
    }
    // ADR 0043 decision 2: `subagent` exists in a parent session only. The depth-1 rule is
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
            Arc::clone(&config_slot),
            Arc::clone(&hooks),
        ));
    }
    // ADR 0036: the system prompt is assembled from layers. 0a <runtime> carries
    // the tier/sandbox facts; 1 <role> is the role
    // prompt (or `--system`); 2 is the context files; 3 is appended by the skills/memory/goal
    // hooks; 0b <session> is appended last by its own hook, so the static prefix stays cacheable.
    let model_name = config_snapshot
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .to_owned();
    let runtime_layer = crate::prompt::runtime_layer(&crate::prompt::Runtime {
        tier: tier.as_str(),
        sandbox: tier_facts,
        // Both tiers leave the network on: `workspace` allows outbound and loopback
        // TCP/UDP (ADR 0016 decision 1), and `full` has no sandbox at all (ADR 0043
        // decision 5). Deriving this from the tier printed `Network: off.` for
        // `full`, against the sandbox facts beside it.
        network: true,
        unix_sockets_restricted,
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
    let goal_hooks = Arc::new(crate::goal::GoalHooks::new(Arc::clone(&goal_share)));
    // The exact base the hook chain writes (layers 0a, 1 and 2): the side-question
    // shape rebuilds the prompt from it, so the bytes match the formal request.
    let role_base = crate::prompt::assemble(&runtime_layer, &role_layer, &context.section);
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
        model_name.clone(),
        Some(Arc::clone(&config_slot)),
    ));
    // The driver's model, shared with the side-question path: `/btw` sends
    // through the same route, thinking level and credentials as the next
    // formal request (`Arc` because `Agent` takes ownership).
    let side_model = Arc::new(model);
    let agent = Arc::new(
        Agent::new(Arc::clone(&side_model), tools)
            .with_config(agent_config)
            .with_hooks(session_hooks(
                Arc::clone(&role_hooks),
                Arc::clone(&hooks),
                Arc::clone(&command_hooks),
                Arc::clone(&memory_hooks),
                Arc::clone(&skills_hooks),
                Arc::clone(&goal_hooks),
                Arc::clone(&facts_hooks),
            )),
    );

    // MCP server **processes** start here, when the tool session opens. The session file
    // already exists, so a failure names the way to continue it.
    // A written abort changes the file on disk, so the shared parse is stale: re-parse then.
    //
    // Today an abort does not change the **fold**: an uncommitted segment at end of file and an
    // aborted one both drop the segment and leave the history and the declared calls alone
    // (`radian_core::TranscriptFold`; `an_aborted_segment_and_one_at_end_of_file_fold_identically`
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
    driver.set_runtime_events(Arc::clone(&runtime_events));
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
    // ADR 0027 slice ③ item 5: it also forwards the start/settle pair on the
    // live runtime-event sink, which is how the TUI's footer `jobs` item reads them (never
    // by polling the journal file).
    let turn_running = driver.turn_running_handle();
    let idle = driver.idle_signal();
    let job_notifier = jobs.as_ref().map(|jobs| {
        let notifier = crate::job_notify::JobNotifier::new(
            Arc::clone(jobs),
            Arc::clone(&inbox),
            Arc::clone(&journal),
            Arc::clone(&turn_running),
            Arc::clone(&runtime_events),
            wake_warning,
            notice_lost_warning,
        );
        // ADR 0011: a stranded notice that opens the next turn counts one
        // wake. The inbox fires the callback without holding its lock, and the `Weak`
        // keeps the inbox from owning the session's notifier (which owns the inbox).
        let weak = Arc::downgrade(&notifier);
        inbox.set_pending_wake_callback(Some(Arc::new(move |_| {
            if let Some(notifier) = weak.upgrade() {
                notifier.note_promoted_wake();
            }
        })));
        notifier.spawn(&runtime);
        notifier
    });

    // Backlog `turn-end-continuation.md` §B: the built-in validator's context. The jobs
    // registry answers "no background job running", the shared goal suppresses the
    // validator while a goal owns the turn end, and the TUI loop syncs `/loop` the same
    // way (exec has no loop, so nothing sets it there).
    command_hooks.set_jobs(jobs.as_ref());
    command_hooks.set_goal(Arc::clone(&goal_share));

    // ADR 0037 decision 6: a start with no settle after a crash is reported once as unknown
    // and never re-run. Injected (no wake), so the model sees it on the next turn.
    if let Some(parsed) = parsed.as_ref() {
        for (id, command) in crate::job_notify::orphan_jobs(parsed) {
            let _ = inbox
                .inject(
                    crate::job_notify::orphan_message(&id, &command),
                    radian_core::InputSource::System,
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
    let tool_specs = spec_sink
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default();
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
    // The `/context` estimate inputs: the system layers split so the categories
    // add up (base is everything except the context section and the memory
    // block, which are counted separately), the per-file blocks, the skills
    // listing, and the session facts. Read-only; the loop reads them while a
    // turn runs.
    let system_base_prefix = crate::prompt::assemble(&runtime_layer, &role_layer, "");
    let skills_section = skills_hooks.section().to_owned();
    let skills_count = shared_skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .count();
    let session_section = crate::prompt::session_layer(&workspace, &model_name);
    let system_base = format!("{system_base_prefix}{skills_section}\n{session_section}");
    let context_blocks = context
        .blocks
        .iter()
        .map(|block| {
            (
                block.path.to_string_lossy().into_owned(),
                block.block.clone(),
            )
        })
        .collect();
    let context_section = context.section.clone();
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
        command_hooks,
        sandbox_slot,
        access_builder,
        turn_running,
        idle,
        projection,
        queue,
        memory,
        skills: shared_skills,
        goal: goal_share,
        job_notifier,
        jobs,
        tool_specs,
        system_base,
        role_base,
        facts_hooks,
        side_model,
        price_for,
        context_section,
        context_blocks,
        skills_section,
        skills_count,
        memory_injection: Arc::clone(&injection),
        context_budget,
        window_context,
        mcp_servers: mcp_server_configs,
        mcp_config_path,
        hooks_config,
        project_hooks_config,
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
