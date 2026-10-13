//! The `radian-tui` binary: one real conversation through `radian-runtime`, rendered full
//! screen.
//!
//! Never run this in a shared terminal by hand without expecting it to take over: it enters
//! the alternate screen and raw mode. The tests drive it through a pty.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use agent_core::{
    Agent, AgentConfig, AgentEvent, ContextBudget, EventSink, InputSource, TurnOptions,
};
use agent_extensions::McpToolProvider;
use clap::Parser;
use radian_coding::CodingTools;
use radian_host::memory::{InjectionConfig, MemoryHooks, MemoryService, MemoryTools, ProjectState};
use radian_host::{
    DEFAULT_MAX_TOKENS, HostTools, ModelArgs, Provider, Thinking, build_model,
    check_provider_flags, load_mcp_config, parse_headers, validate_session_id,
};
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalRequest, Approver, AuditSink, PermissionHooks,
    PermissionResolved, runtime_event,
};
use radian_runtime::{CancelCause, Driver, Inbox};
use radian_sandbox::Sandbox;
use radian_session::{Index, SessionCreatedData, SessionJournal};
use radian_tui::app::{App, ApprovalPrompt, Send};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

#[derive(Parser, Debug)]
#[command(
    name = "radian-tui",
    version,
    about = "Full-screen TUI for the radian agent runtime"
)]
struct Cli {
    #[arg(long)]
    model: String,

    /// Provider wire protocol: `openai-compatible` (the default, unchanged) or `anthropic`.
    #[arg(long, value_name = "PROVIDER")]
    provider: Option<String>,

    /// Everything before the provider's path (`/chat/completions` for openai-compatible,
    /// `/v1/messages` for anthropic). Defaults to the provider's public endpoint.
    #[arg(long, value_name = "URL")]
    base_url: Option<String>,

    /// Environment variable holding the API key. Never read from anywhere else, never
    /// printed. Defaults to `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`.
    #[arg(long, value_name = "VAR")]
    api_key_env: Option<String>,

    /// `max_tokens` for anthropic (the Messages API requires it). Ignored for
    /// openai-compatible. Defaults to 16384.
    #[arg(long, value_name = "N")]
    max_tokens: Option<u32>,

    /// Extended thinking for anthropic: `off` (default), `adaptive`, or `enabled:<budget>`.
    /// Ignored for openai-compatible.
    #[arg(long, value_name = "MODE")]
    thinking: Option<String>,

    /// Extra request header, repeatable: `--header NAME=VALUE`.
    #[arg(long = "header", value_name = "NAME=VALUE")]
    header: Vec<String>,

    /// Where session files live. Defaults to `RADIAN_SESSION_DIR`, then `~/.radian/sessions`
    /// (`$RADIAN_HOME/sessions` when that is set, ADR 0020). The same rule as `agent-cli`.
    #[arg(long, value_name = "DIR")]
    session_dir: Option<PathBuf>,

    /// A stable session identifier: the session file's name. Generated once per process when
    /// omitted.
    #[arg(long, value_name = "VALUE", conflicts_with = "resume")]
    session_id: Option<String>,

    /// Resume an existing session by id, rendering its history into the transcript.
    #[arg(long, value_name = "SESSION_ID", conflicts_with = "list")]
    resume: Option<String>,

    /// List this workspace's sessions and exit.
    #[arg(long)]
    list: bool,

    /// Show a full-screen picker for this workspace's sessions before opening one (Phase 4):
    /// Up/Down select, type to filter, Tab full-text search, Enter resume, n new, Esc quit.
    /// Conflicts with `--resume`/`--session-id`; it does not create a session file.
    #[arg(long, conflicts_with_all = ["resume", "session_id"])]
    pick: bool,

    /// Expose the built-in file and read-only git tools rooted at this directory
    /// (read/write/patch/list, git_status/git_diff). No shell is provided.
    #[arg(long, value_name = "DIR")]
    workspace: Option<PathBuf>,

    /// A JSON file listing MCP stdio servers to start (`{"servers": [...]}`), the same format
    /// `agent-cli` reads. Under `workspace` their tools ask before running and the
    /// servers themselves are **not** sandboxed (ADR 0012 option B).
    #[arg(long, value_name = "FILE")]
    mcp_config: Option<String>,

    /// The permission tier (ADR 0016): `safe` denies file modification, `workspace`
    /// allows writes that resolve inside the workspace, `full` also allows MCP.
    #[arg(long, value_name = "TIER", default_value = "workspace")]
    profile: String,

    /// How `ask` is resolved (ADR 0012's ApprovalPolicy): `on-ask` asks when a rule says so,
    /// `always` asks for every call a rule does not deny, `never` turns `ask` into a denial.
    #[arg(long, value_name = "POLICY", default_value = "on-ask")]
    approval: String,

    /// Approve every tool call without asking, like `agent --yes`. Not "allow everything":
    /// `deny` rules still deny and the sandbox still applies; only the question is removed.
    #[arg(long)]
    yes: bool,

    /// The per-turn model call limit. The core's default (`DEFAULT_MAX_MODEL_CALLS`, 32) is a
    /// fuse for unattended runs; an interactive coding session reads many files per turn and
    /// a human can cancel, so the TUI defaults much higher. It is still a fuse: a loop of
    /// small calls is not stopped by the context budget (which only triggers compaction).
    #[arg(long, value_name = "N", default_value_t = TUI_DEFAULT_MAX_MODEL_CALLS)]
    max_model_calls: u32,

    /// Allow a shell command prefix without asking, repeatable (e.g. `--allow-shell-command
    /// "git status"`). Only a *simple* command matches: any shell metacharacter keeps the
    /// call at `ask`. Write these at the subcommand level, never at a bare program name.
    #[arg(long = "allow-shell-command", value_name = "PREFIX")]
    allow_shell_command: Vec<String>,

    /// The model's context window in tokens (ADR 0015). Omitted, the models.dev catalog is
    /// consulted for `(provider, model)` (ADR 0018); an explicit value always wins. With
    /// neither, the budget is off and the status bar says so.
    #[arg(long, value_name = "TOKENS")]
    context_window: Option<u64>,

    /// Never fetch the models.dev catalog; use only the local cache.
    #[arg(long)]
    offline_catalog: bool,

    /// The models.dev provider id to look the model up under (default: derived from the
    /// `--base-url` hostname; e.g. `opencode.ai` → `opencode`).
    #[arg(long, value_name = "PROVIDER")]
    catalog_provider: Option<String>,

    /// Turn automatic compaction off. Over the input budget `B` the request is then sent
    /// unchanged (ADR 0018).
    #[arg(long)]
    no_auto_compact: bool,

    #[arg(long)]
    system: Option<String>,

    /// Test hook: enter the terminal, then panic, so the restore hook can be exercised.
    #[arg(long, hide = true)]
    panic_after_enter: bool,

    /// Test hook: enter the terminal, then panic with this exact message. Used to prove the
    /// panic log redacts header values and the key from the panic text.
    #[arg(long, hide = true, value_name = "TEXT")]
    panic_message: Option<String>,

    /// Test hook: start a `sleep` child in its own process group, write its pid to
    /// `<session-dir>/child.pid`, then panic. The pty test asserts the child is not orphaned.
    #[arg(long, hide = true, value_name = "SECS")]
    panic_with_child: Option<u64>,

    /// Test hook: make the driver task panic instead of running, so the UI's watch on it can
    /// be exercised.
    #[arg(long, hide = true)]
    panic_in_driver: bool,
}

/// The TUI's per-turn model call limit when `--max-model-calls` is not given (decision
/// 2026-10-12, dogfooding finding D6).
const TUI_DEFAULT_MAX_MODEL_CALLS: u32 = 1000;

/// Forwards core events to the UI thread. `emit` must be short and non-blocking (ADR 0010),
/// so it only does a channel send.
struct ChannelSink(Sender<AgentEvent>);

impl EventSink for ChannelSink {
    fn emit(&self, event: AgentEvent) {
        let _ = self.0.send(event);
    }
}

fn main() -> ExitCode {
    radian_tui::terminal::install_panic_hook();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("radian-tui: {e}");
            return ExitCode::from(1);
        }
    };
    // SIGTERM/SIGHUP take the ordinary exit path from here on (dogfooding D7).
    if let Err(e) = radian_tui::signals::listen(runtime.handle()) {
        eprintln!("radian-tui: cannot listen for SIGTERM/SIGHUP: {e}");
        return ExitCode::from(1);
    }
    // A panic must not unwind through a runtime *drop*: that drop can wait on a parked task
    // and hang. Catch it here, shut the runtime down with a bound (which drops the in-flight
    // tool futures, and with them the `kill_on_drop` child processes they own), then exit
    // 101. The panic hook restores the terminal and returns; it no longer exits, because
    // exiting here would skip exactly that cleanup.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(cli, &runtime)));
    runtime.shutdown_timeout(Duration::from_secs(2));
    match result {
        Ok(Ok(reason)) => {
            // A clean exit still says why, on the normal screen (dogfooding D4). `--list`
            // prints its own output and passes `None`.
            if let Some(reason) = reason {
                eprintln!("radian-tui: {reason}");
            }
            // A signal-driven exit is still an orderly one, but the caller asked us to stop:
            // report it the way a shell does, 128 + the signal number.
            match radian_tui::signals::received() {
                Some(signal) => ExitCode::from((128 + signal) as u8),
                None => ExitCode::SUCCESS,
            }
        }
        Ok(Err(message)) => {
            eprintln!("radian-tui: {message}");
            ExitCode::from(1)
        }
        Err(_) => {
            let mut message = "the app panicked".to_string();
            if let Some(panic) = radian_tui::terminal::last_panic() {
                message.push_str(": ");
                message.push_str(&panic);
            }
            // D4: say where the full panic text went.
            if let Some(path) = radian_tui::terminal::panic_log_path() {
                message.push_str(&format!(" (panic log: {})", path.display()));
            }
            eprintln!("radian-tui: {message}");
            ExitCode::from(101)
        }
    }
}

fn run(mut cli: Cli, runtime: &tokio::runtime::Runtime) -> Result<Option<String>, String> {
    if cli.list {
        return list_sessions(&cli).map(|()| None);
    }
    // ADR 0013 decision 4: diagnostics live in the user cache directory, and cleanup runs at
    // startup. A failure is one line, never a fault.
    radian_host::clean_startup();
    let dir = session_dir(&cli)?;
    // ADR 0020 decision 5: never move a user's data; print a copy-pasteable command instead.
    if cli.session_dir.is_none()
        && std::env::var("RADIAN_SESSION_DIR").map_or(true, |dir| dir.is_empty())
        && let Some(hint) = radian_host::legacy_sessions_hint()
    {
        eprintln!("{hint}");
    }
    // Phase 4: choose a session before the session id (and any header that expands it) is
    // resolved, and before any session file exists. A `Resume` becomes the ordinary
    // `--resume`; `Quit` exits with nothing created.
    let mut pick_notice = false;
    if cli.pick {
        let workspace = radian_host::resolve_workspace(cli.workspace.as_deref())?
            .to_string_lossy()
            .into_owned();
        match pick_session(&dir, &workspace)? {
            // No sessions yet: go straight to a new one, with a notice (a list of nothing is
            // not worth a full-screen flash).
            None => pick_notice = true,
            Some(radian_tui::picker::Outcome::Quit) => return Ok(None),
            Some(radian_tui::picker::Outcome::New) => {}
            Some(radian_tui::picker::Outcome::Resume(id)) => cli.resume = Some(id),
        }
    }
    // The session id is resolved before the headers: a `{session_id}` in a header expands to
    // this process's id, and `--resume` uses the id being resumed. The parser is the same one
    // `agent-cli` uses, so `{session_id}`, name validation and the `authorization` refusal
    // behave identically in both hosts.
    let session_id = match (&cli.resume, &cli.session_id) {
        (Some(id), _) | (None, Some(id)) => id.clone(),
        (None, None) => radian_host::generate_session_id(),
    };
    validate_session_id(&session_id)?;
    // The provider names the defaults; an explicit flag always wins.
    let provider = Provider::from_name(cli.provider.as_deref().unwrap_or("openai-compatible"))?;
    let base_url = cli
        .base_url
        .clone()
        .unwrap_or_else(|| provider.default_base_url().to_owned());
    let api_key_env = cli
        .api_key_env
        .clone()
        .unwrap_or_else(|| provider.default_api_key_env().to_owned());
    let thinking = match cli.thinking.as_deref() {
        Some(value) => value.parse::<Thinking>()?,
        None => Thinking::Off,
    };
    // `--thinking` and `--max-tokens` are Anthropic-only; under openai-compatible they would
    // be ignored, so this is an error before the session file exists (dogfooding D15).
    check_provider_flags(provider, thinking, cli.max_tokens)?;
    let headers = parse_headers(&cli.header, &session_id)?;
    // Header values and the key must not reach the panic log (dogfooding D4).
    let mut secrets: Vec<String> = headers.iter().map(|(_, value)| value.clone()).collect();
    let key = std::env::var(&api_key_env).ok().filter(|k| !k.is_empty());
    if let Some(key) = &key {
        secrets.push(key.clone());
    }
    // D1/D2: every check that can fail runs **before** the session file is created. The
    // workspace is validated first, so a typo is a workspace error and never reaches the
    // sandbox probe (dogfooding D1). `radian-host` owns the rule so both hosts share it.
    let workspace_path = radian_host::resolve_workspace(cli.workspace.as_deref())?;
    let catalog_base_url = base_url.clone();
    let model = build_model(ModelArgs {
        provider,
        base_url,
        model: cli.model.clone(),
        api_key: key,
        headers,
        // The TUI has no `--include-usage` flag; the adapter's default policy applies.
        include_usage: None,
        max_tokens: cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        thinking,
    })?;
    // The permission tier and the sandbox. For an enforced tier a probe failure is an error
    // here, never a fallback to an unsandboxed run (ADR 0012 decision 2). Still before
    // the session file exists, so a refusal leaves nothing behind (dogfooding D2).
    let tier = radian_tui::profile::parse_tier(&cli.profile)?;
    // ADR 0020 decision 3: the sandbox denies read on the **actual** sessions directory and
    // the memory directory. Create them owner-only first so Linux's bwrap (which can only mask
    // paths that exist) can hide them.
    let _ = radian_host::ensure_private_dir(&dir);
    let memory_root = radian_host::memory_dir();
    let mut sandbox_deny_read = vec![dir.clone()];
    if let Some(memory) = &memory_root {
        sandbox_deny_read.push(memory.clone());
    }
    let sandbox = radian_tui::profile::build_sandbox(tier, &workspace_path, &sandbox_deny_read)?;
    // The description is taken before the sandbox moves into the tools.
    let (startup, meaning) = radian_tui::profile::describe(tier, sandbox.as_ref());
    // The MCP config file is read and validated here: before any server process starts and
    // before a session file exists, so a bad file leaves nothing behind (dogfooding D2).
    let servers = match &cli.mcp_config {
        Some(path) => load_mcp_config(path)?,
        None => Vec::new(),
    };
    let mcp = McpToolProvider::new(servers);
    let mcp_configured = mcp.configured();
    let coding = build_coding_tools(cli.workspace.as_deref(), sandbox)?;
    // ADR 0021: the same service `agent memory` uses, plus the agent tools and the injection
    // hook. `None` only when neither `RADIAN_HOME` nor `HOME` is set.
    let injection_config = InjectionConfig::default();
    let memory_service = memory_root
        .as_ref()
        .map(|root| MemoryService::open(&workspace_path, root));
    let memory_tools = memory_service
        .as_ref()
        .map(|service| MemoryTools::new(service.clone()));
    let memory_notice = match &memory_service {
        Some(service) => match service.state() {
            Ok(ProjectState::Known(project)) => format!(
                "[memory] project {project}; injection on, up to {} tokens",
                injection_config.max_tokens
            ),
            Ok(ProjectState::Unlinked { .. }) => format!(
                "[memory] no project linked; global memory only (`agent memory project new` or \
                 `relink`); injection on, up to {} tokens",
                injection_config.max_tokens
            ),
            Err(error) => format!("[memory] unavailable: {error}"),
        },
        None => "[memory] disabled (no RADIAN_HOME or HOME)".to_owned(),
    };
    let approval = match cli.approval.as_str() {
        "on-ask" => radian_permissions::ApprovalPolicy::OnAsk,
        "always" => radian_permissions::ApprovalPolicy::Always,
        "never" => radian_permissions::ApprovalPolicy::Never,
        other => {
            return Err(format!(
                "--approval {other:?} is not one of on-ask, always, never"
            ));
        }
    };
    let profile =
        radian_tui::profile::build_profile(tier, &workspace_path, &cli.allow_shell_command)?
            .with_approval(approval);

    // The window comes from `--context-window` or the models.dev catalog (ADR 0018). A fetch
    // failure is not a startup failure: the window is then unknown and the budget stays off.
    // Still before the session file exists. `R` comes from the request's own output setting:
    // Anthropic sends `max_tokens`, OpenAI-compatible sends none.
    let request_max_output = match provider {
        Provider::Anthropic => Some(cli.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        Provider::OpenAiCompatible => None,
    };
    let window = runtime.block_on(radian_host::resolve(&radian_host::WindowQuery::production(
        cli.context_window,
        cli.catalog_provider.clone(),
        &catalog_base_url,
        cli.model.clone(),
        request_max_output,
        cli.offline_catalog,
    )));
    // ADR 0018: `C ≤ R` refuses an explicit `--context-window` and only warns for a catalog
    // window (the budget is then off).
    match window.startup_check() {
        radian_host::WindowCheck::Ok => {}
        radian_host::WindowCheck::Refuse(message) => return Err(message),
        radian_host::WindowCheck::Warn(message) => eprintln!("{message}"),
    }
    if let Some(warning) = radian_host::output_ceiling_warning(request_max_output, window.output) {
        eprintln!("{warning}");
    }

    // Every startup check that can fail is done. Only now is the session file created; a
    // failure above leaves the session directory untouched (dogfooding D2).
    let journal = open_journal(
        &cli,
        &session_id,
        &dir,
        &workspace_path.to_string_lossy(),
        provider,
    )?;
    // D4: panics after this point get a log in the diagnostics directory (ADR 0013 decision
    // 4), not next to the session: the session is the authoritative record and must not be
    // treated as a cache. Set before the terminal is entered; the hook reads it on a panic.
    let panic_log = radian_host::diagnostics_dir().map(|dir| {
        dir.join(radian_host::panic_log_name(
            &session_id,
            radian_host::now_millis(),
        ))
    });
    let secrets_for_errors = Arc::new(secrets.clone());
    radian_tui::terminal::set_panic_log(panic_log, secrets);
    // The history of a resumed session is rendered before the first frame, so the transcript
    // shows it from the start. `Driver::open` recovers the same history from the journal for
    // the model; this is only the view of it.
    let history = if cli.resume.is_some() {
        radian_session::Session::open(dir.join(format!("{session_id}.jsonl")))
            .and_then(|session| session.history())
            .map_err(|e| format!("cannot read session {session_id:?}: {e}"))?
    } else {
        Vec::new()
    };
    let inbox = Inbox::durable(64, Arc::clone(&journal));

    let (tx, rx) = std::sync::mpsc::channel();
    let live: Arc<dyn EventSink> = Arc::new(ChannelSink(tx));
    let agent_config = AgentConfig {
        system_prompt: cli.system.clone(),
        // No window known means the budget is off, never a guessed default (ADR 0015); the
        // window comes from the catalog or `--context-window` (ADR 0018).
        context_budget: window.budget.map(ContextBudget::new),
        auto_compact: !cli.no_auto_compact,
        ..AgentConfig::default()
    };

    // Tools, permissions and approval. The permission layer decides allow/ask/deny; this
    // process is the host that answers `ask` through the UI, and writes every decision to the
    // session journal as a `PermissionResolved` (audit only, ADR 0006 §5.9).
    let (approval_tx, approval_rx) = std::sync::mpsc::channel::<ApprovalPrompt>();
    // `--yes` swaps the approver for one that answers "allow once" without asking; rules,
    // the sandbox and the audit record are untouched by it (same as `agent --yes`).
    let approver: Arc<dyn Approver> = if cli.yes {
        Arc::new(AutoApprover)
    } else {
        Arc::new(TuiApprover { tx: approval_tx })
    };
    let hooks = Arc::new(PermissionHooks::new(
        profile,
        Some(approver),
        Arc::new(JournalAudit {
            journal: Arc::clone(&journal),
            runtime: runtime.handle().clone(),
        }),
    ));
    // ADR 0021 D6: injection is the second hook in the chain; a memory failure is a notice,
    // never a failed turn.
    let memory_hooks = Arc::new(MemoryHooks::new(
        memory_service.clone(),
        Arc::clone(&journal),
        injection_config,
        Arc::new(|message: &str| eprintln!("radian-tui: [memory] {message}")),
    ));
    let agent = Arc::new(
        Agent::new(model, HostTools::new(coding, memory_tools, mcp.clone()))
            .with_config(agent_config)
            .with_hooks((Arc::clone(&hooks), Arc::clone(&memory_hooks))),
    );

    // MCP server **processes** start here, when the tool session opens. The session file
    // already exists, so the failure is reported with the way to continue it; the file is
    // kept because its durable content is real and a later `--resume` can use it.
    let journal_path = dir.join(format!("{session_id}.jsonl"));
    let mut driver = runtime
        .block_on(Driver::open(
            &agent,
            Arc::clone(&journal),
            Arc::clone(&inbox),
            live,
            TurnOptions {
                max_model_calls: Some(cli.max_model_calls),
                ..TurnOptions::default()
            },
        ))
        .map_err(|e| {
            format!(
                "driver: {e}\n  the session file {} exists; after fixing the problem, \
                 continue it with `--resume {session_id}`",
                journal_path.display()
            )
        })?;

    // ADR 0017: the driver reports a failed turn over a runtime-event channel. The UI shows
    // the (host-redacted) message as a transcript notice and stays usable; the driver keeps
    // running. `AgentEvent` is unchanged, so this is a separate channel.
    let (runtime_tx, runtime_rx) = std::sync::mpsc::channel::<radian_session::RuntimeEvent>();
    let secrets_for_events = Arc::clone(&secrets_for_errors);
    driver.set_runtime_events(Arc::new(move |event: &radian_session::RuntimeEvent| {
        let redacted = match event {
            radian_session::RuntimeEvent::TurnFailed { turn_id, message } => {
                radian_session::RuntimeEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    message: radian_tui::terminal::redact(message, &secrets_for_events),
                }
            }
            other => other.clone(),
        };
        let _ = runtime_tx.send(redacted);
    }));

    // The session is open now, so the MCP status is known: the servers and tools that actually
    // connected, not just what was configured.
    let mcp_open = mcp.status().map(|status| (status.servers, status.tools));

    // The driver runs until the UI asks it to stop; dropping its `run()` future releases the
    // borrow so `shutdown()` can be called.
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let panic_in_driver = cli.panic_in_driver;
    let driver_task = runtime.spawn(async move {
        if panic_in_driver {
            panic!("radian-tui: driver task panic test");
        }
        tokio::select! {
            result = driver.run() => result,
            _ = &mut stop_rx => {
                driver.shutdown();
                driver.run().await
            }
        }
    });

    let mut guard =
        radian_tui::terminal::TtyGuard::enter().map_err(|e| format!("terminal: {e}"))?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))
        .map_err(|e| format!("terminal: {e}"))?;
    if cli.panic_after_enter {
        panic!("radian-tui: panic path test");
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
        panic!("radian-tui: panic path test with a child in flight");
    }
    let mut app = App::new(cli.model.clone(), session_id.clone());
    // `Tab` completes `@` file references against the workspace.
    app.set_workspace(workspace_path.clone());
    app.set_context_window(window.budget);
    app.set_tier(tier.as_str());
    app.set_mcp(mcp_configured, mcp_open);
    // `/memory` runs against the same service the agent's tools and injection use.
    app.set_memory(memory_service.clone());
    for message in &history {
        app.push_message(message);
    }
    app.prefill_history(&history);
    // A draft from a previous exit is restored once, when resuming the same session.
    if cli.resume.is_some() {
        let path = radian_tui::composer::draft_path(&dir, &session_id);
        match radian_tui::composer::take_draft(&path) {
            Ok(Some(text)) => app.input.set_text(&text),
            Ok(None) => {}
            Err(error) => eprintln!("[draft] cannot read {}: {error}", path.display()),
        }
    }
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        format!("profile: {startup}"),
    ));
    // ADR 0012 decision 4: the meaning line must not claim credential isolation.
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        meaning,
    ));
    // ADR 0018: the window and exactly where it came from.
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        format!("context: {}", window.describe()),
    ));
    // ADR 0021 D2/D6: the project state and the injection budget.
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        memory_notice,
    ));
    if cli.yes {
        app.entries.push(radian_tui::Entry::new(
            radian_tui::EntryKind::Notice,
            "--yes: tool calls are approved without asking; deny rules and the sandbox still apply",
        ));
    }
    if pick_notice {
        app.entries.push(radian_tui::Entry::new(
            radian_tui::EntryKind::Notice,
            "no sessions for this workspace yet; starting a new one",
        ));
    }
    app.entries.push(radian_tui::Entry::new(
        radian_tui::EntryKind::Notice,
        match &cli.resume {
            Some(id) => format!(
                "resumed session {id} ({} messages) · Enter sends · Ctrl-C cancels a turn · \
                 Ctrl-C again quits",
                history.len()
            ),
            None => format!(
                "new session {session_id} · Enter sends · Ctrl-C cancels a turn · Ctrl-C again \
                 quits"
            ),
        },
    ));

    let session_host = SessionHost {
        dir: dir.clone(),
        workspace: workspace_path.to_string_lossy().into_owned(),
        session_id: session_id.clone(),
        secrets: secrets_for_errors.as_ref().clone(),
    };
    let outcome = ui_loop(
        &mut terminal,
        &mut app,
        &workspace_path,
        &rx,
        &runtime_rx,
        &approval_rx,
        runtime,
        &inbox,
        &hooks,
        &driver_task,
        &session_host,
    );
    guard.restore();
    // The draft is written on every clean exit (normal quit, Ctrl-C, SIGTERM/SIGHUP). A panic
    // never reaches here, so the panic path does no file I/O.
    let draft = radian_tui::composer::draft_path(&dir, &session_id);
    if let Err(error) = radian_tui::composer::save_draft(&draft, &app.input.text()) {
        eprintln!("[draft] cannot write {}: {error}", draft.display());
    }

    let _ = stop_tx.send(());
    // The driver task is the session: if it died, the failure is the app's failure. A panic
    // inside it was caught by the runtime, so it shows up here as a `JoinError`, and the
    // message the hook recorded is printed now that the alternate screen is gone.
    let driver_result = runtime.block_on(driver_task);
    match driver_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(format!("driver: {error}")),
        Err(join_error) => {
            let mut message = format!("the driver task ended unexpectedly: {join_error}");
            if let Some(panic) = radian_tui::terminal::last_panic() {
                // One line: the normal screen gets one reason line (dogfooding D4).
                message.push_str(": ");
                message.push_str(&panic);
            }
            return Err(message);
        }
    }
    // A failure in the UI loop is the app's failure too; the terminal is already restored.
    outcome?;
    let saved = format!("session {session_id} saved; resume with `--resume {session_id}`");
    Ok(Some(match radian_tui::signals::received() {
        Some(signal) => format!("received {}; {saved}", radian_tui::signals::name(signal)),
        None => saved,
    }))
}

#[allow(clippy::too_many_arguments)]
fn ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    workspace: &Path,
    rx: &Receiver<AgentEvent>,
    runtime_events: &Receiver<radian_session::RuntimeEvent>,
    approvals: &Receiver<ApprovalPrompt>,
    runtime: &tokio::runtime::Runtime,
    inbox: &Arc<Inbox>,
    hooks: &Arc<PermissionHooks>,
    driver_task: &tokio::task::JoinHandle<Result<(), radian_runtime::DriverError>>,
    host: &SessionHost,
) -> Result<(), String> {
    // Manual `/compact` runs on the runtime (the driver executes it between turns); its
    // result arrives over these channels and becomes a notice.
    let mut pending_compacts: Vec<std::sync::mpsc::Receiver<Result<_, String>>> = Vec::new();
    while !app.quit {
        // SIGTERM/SIGHUP: leave exactly like a quit, so the terminal is restored and the
        // reason is printed on the normal screen (dogfooding D7). No last frame: after a
        // SIGHUP the terminal may already be gone.
        if radian_tui::signals::received().is_some() {
            app.quit = true;
            break;
        }
        // The driver task is the session. If it ends — normally or by panicking — the UI has
        // nothing left to talk to, so leave in order instead of drawing over a dead session
        // and sending input into a closed inbox. The exit code and the message are decided
        // after the terminal is restored (see `run`).
        if driver_task.is_finished() {
            app.note = Some("the driver stopped; leaving".into());
            app.entries.push(radian_tui::Entry::new(
                radian_tui::EntryKind::Notice,
                "the driver task ended; the session is closed",
            ));
            app.quit = true;
            // One last frame, so the notice is what the user sees before the screen is
            // restored.
            terminal
                .draw(|frame| radian_tui::render::draw(frame, app))
                .map_err(|e| e.to_string())?;
            break;
        }
        // A short poll keeps rendering responsive without blocking on input; it is a UI
        // refresh interval, not a synchronization device.
        if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
            match event::read().map_err(|e| e.to_string())? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if let Some(send) = app.on_key(key) {
                        dispatch(send, app, workspace, runtime, inbox, hooks)?;
                    }
                }
                Event::Paste(text) => app.input.insert_str(&text),
                _ => {}
            }
        }
        while let Ok(event) = rx.try_recv() {
            app.apply_event(&event);
        }
        // ADR 0017: a failed turn is shown in the transcript; the session stays open.
        while let Ok(event) = runtime_events.try_recv() {
            if let radian_session::RuntimeEvent::TurnFailed { message, .. } = event {
                app.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    format!("turn failed: {message}"),
                ));
                // ADR 0018's fallback: a recognizable context overflow gets one extra line,
                // since automatic compaction did not keep the request inside the window.
                // The host only has the text, so the prefix is what identifies it.
                if radian_host::is_context_overflow(&message) {
                    app.entries.push(radian_tui::Entry::new(
                        radian_tui::EntryKind::Notice,
                        radian_host::CONTEXT_OVERFLOW_HELP.to_string(),
                    ));
                }
            }
        }
        // A tool call waiting for the human: show it. The answer goes back over the prompt's
        // own channel when a key is pressed, so this loop never blocks on the decision.
        while let Ok(prompt) = approvals.try_recv() {
            app.request_approval(prompt);
        }
        // `/compact`: hand it to the runtime and show the outcome when it finishes. The
        // driver runs it between turns, so a running turn finishes first.
        if let Some(instructions) = app.pending_compact.take() {
            let inbox = Arc::clone(inbox);
            let (tx, rx) = std::sync::mpsc::channel();
            runtime.spawn(async move {
                let _ = tx.send(inbox.compact(instructions).await);
            });
            pending_compacts.push(rx);
        }
        // `/sessions` and `/export` are host work (the index / a file), not transcript
        // changes; run them here where the session directory, workspace and secrets are known.
        if let Some(request) = app.pending_session.take() {
            handle_session_request(app, host, request);
        }
        pending_compacts.retain(|rx| match rx.try_recv() {
            Ok(Ok(agent_core::CompactionOutcome::Compacted {
                tokens_before,
                tokens_after,
                ..
            })) => {
                app.push_notice(format!(
                    "compaction: {tokens_before} → {tokens_after} tokens"
                ));
                false
            }
            Ok(Ok(agent_core::CompactionOutcome::NothingToCompact)) => {
                app.push_notice("compaction: nothing to compact");
                false
            }
            Ok(Err(message)) => {
                app.push_notice(format!("compaction failed: {message}"));
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
        });
        terminal
            .draw(|frame| radian_tui::render::draw(frame, app))
            .map_err(|e| e.to_string())?;
        // The cursor is visible only while the input box is editable.
        radian_tui::terminal::set_cursor_visible(app.input_editable());
    }
    Ok(())
}

/// The host facts `/sessions` and `/export` need: where sessions live, which workspace this
/// process belongs to, the current session id and the values to redact.
struct SessionHost {
    dir: PathBuf,
    workspace: String,
    session_id: String,
    secrets: Vec<String>,
}

/// Runs one `/sessions` or `/export` request and pushes the result as notices.
fn handle_session_request(
    app: &mut App,
    host: &SessionHost,
    request: radian_tui::command::SessionOutcome,
) {
    match request {
        radian_tui::command::SessionOutcome::List(query) => {
            match session_lines(host, query.as_deref()) {
                Ok(lines) => {
                    for line in lines {
                        app.push_notice(line);
                    }
                }
                Err(message) => app.push_notice(message),
            }
        }
        radian_tui::command::SessionOutcome::Export(request) => {
            match export_current(host, &request) {
                Ok(path) => app.push_notice(format!("exported to {path}")),
                Err(message) => app.push_notice(message),
            }
        }
    }
}

/// The `/sessions` lines: the recent list or the full-text hits, at most 20, each with a
/// copyable `--resume <id>`. A workspace with no sessions (or no matches) is one notice.
fn session_lines(host: &SessionHost, query: Option<&str>) -> Result<Vec<String>, String> {
    let Some(index) = radian_host::sessions::open_index(&host.dir)? else {
        return Ok(vec!["no sessions for this workspace yet".to_owned()]);
    };
    let mut lines = Vec::new();
    match query {
        Some(query) => {
            let hits = radian_host::sessions::search(&index, &host.workspace, query, 20)?;
            if hits.is_empty() {
                return Ok(vec![format!("no match for {query:?}")]);
            }
            for hit in hits {
                let snippet = radian_tui::terminal::redact(&hit.snippet, &host.secrets);
                lines.push(format!(
                    "{}  {}  {}  --resume {}",
                    hit.session_id,
                    hit.last_ts.as_deref().unwrap_or("-"),
                    snippet,
                    hit.session_id
                ));
            }
        }
        None => {
            let rows = radian_host::sessions::list(&index, &host.workspace)?;
            if rows.is_empty() {
                return Ok(vec!["no sessions for this workspace yet".to_owned()]);
            }
            for row in rows.into_iter().take(20) {
                lines.push(format!(
                    "{}  {}  {} messages  {}  --resume {}",
                    row.session_id,
                    row.last_ts.as_deref().unwrap_or("-"),
                    row.message_count,
                    row.summary,
                    row.session_id
                ));
            }
        }
    }
    lines.push(
        "this process does not switch sessions in place; reopen with `--resume <id>` or `--pick`"
            .to_owned(),
    );
    Ok(lines)
}

/// `/export`: render the current session with the shared host layer, check overwrite, and
/// write atomically. Returns the absolute path for the notice.
fn export_current(
    host: &SessionHost,
    request: &radian_tui::command::ExportRequest,
) -> Result<String, String> {
    let extension = request.format.extension();
    let path = match &request.path {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                path
            } else {
                PathBuf::from(&host.workspace).join(path)
            }
        }
        None => {
            PathBuf::from(&host.workspace).join(format!("radian-{}.{extension}", host.session_id))
        }
    };
    if path.exists() && !request.force {
        return Err(format!(
            "{} already exists; `/export {} {} --force` overwrites it",
            path.display(),
            match request.format {
                radian_host::Format::Markdown => "md",
                radian_host::Format::Json => "json",
            },
            path.display()
        ));
    }
    let settings = radian_host::ExportSettings {
        include_reasoning: false,
        include_provider_payload: false,
        secrets: host.secrets.clone(),
    };
    let rendered = radian_host::sessions::export(
        &host.dir,
        &host.workspace,
        &host.session_id,
        request.format,
        &settings,
    )?;
    write_atomic(&path, &rendered)?;
    Ok(std::fs::canonicalize(&path)
        .unwrap_or(path)
        .display()
        .to_string())
}

/// Writes `contents` to `path` atomically: a sibling temp file, then a rename. 0644 (the file
/// is for the user; it is not a session file).
fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    static EXPORT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let seq = EXPORT_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = parent.join(format!(".radian-export-{}-{seq}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let mut file = options.open(&tmp)?;
        use std::io::Write;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", path.display()));
    }
    Ok(())
}

fn dispatch(
    send: Send,
    app: &mut App,
    workspace: &Path,
    runtime: &tokio::runtime::Runtime,
    inbox: &Arc<Inbox>,
    hooks: &Arc<PermissionHooks>,
) -> Result<(), String> {
    match send {
        Send::Followup(text) => {
            // `@` references expand here, before the text reaches the inbox, so the journal
            // stores exactly what the model sees. A failed expansion is not sent and the
            // input is restored.
            let Some(text) = prepare_message(app, workspace, text) else {
                return Ok(());
            };
            // `HookContext` has no source field, so the host declares each turn's source
            // before it starts; an unset source is treated as non-human and denied.
            hooks.set_turn_source(Some(InputSource::User));
            runtime
                .block_on(inbox.followup(text, InputSource::User))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Send::Steer(text) => {
            let Some(text) = prepare_message(app, workspace, text) else {
                return Ok(());
            };
            hooks.set_turn_source(Some(InputSource::User));
            runtime
                .block_on(inbox.steer(text, InputSource::User))
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Send::Cancel => {
            inbox.cancel(CancelCause::Requested);
            Ok(())
        }
        Send::Quit => Ok(()),
    }
}

/// Expands the `@` references in a user message.
///
/// On success, shows the `attached N file(s)` notice (and a warning per reference that
/// resolved outside the workspace) and returns the text to send. On failure, shows the
/// reasons and puts the original text back in the input box; returns `None`, so nothing is
/// sent and the user can fix the message.
fn prepare_message(app: &mut App, workspace: &Path, text: String) -> Option<String> {
    match radian_tui::mention::expand(&text, workspace) {
        Ok(expansion) => {
            for warning in &expansion.warnings {
                app.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    warning.clone(),
                ));
            }
            if !expansion.attached.is_empty() {
                app.entries.push(radian_tui::Entry::new(
                    radian_tui::EntryKind::Notice,
                    format!(
                        "attached {} file(s): {}",
                        expansion.attached.len(),
                        expansion.attached.join(", ")
                    ),
                ));
            }
            Some(expansion.text)
        }
        Err(error) => {
            app.entries.push(radian_tui::Entry::new(
                radian_tui::EntryKind::Notice,
                format!("not sent: {error}"),
            ));
            app.restore_input(text);
            None
        }
    }
}

/// Where session files live: `--session-dir`, then `RADIAN_SESSION_DIR`, then the platform's
/// *data* directory (never a cache or temp path; ADR 0013 decision 4). This is the same
/// convention `agent-cli` uses, so a session written by one is resumable by the other.
fn session_dir(cli: &Cli) -> Result<PathBuf, String> {
    if let Some(dir) = &cli.session_dir {
        return Ok(dir.clone());
    }
    if let Ok(dir) = std::env::var("RADIAN_SESSION_DIR")
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    // ADR 0020: the default is `~/.radian/sessions` (or `$RADIAN_HOME/sessions`).
    radian_host::sessions_dir().ok_or_else(|| {
        "no RADIAN_HOME or HOME to place sessions under; pass --session-dir".to_string()
    })
}

/// The workspace a session belongs to: `--workspace` when given, else the current directory,
/// canonicalized and validated by the shared `radian-host` rule. `--list` filters by the same
/// value, so a session is only listed in the workspace it was created in, and a bad
/// `--workspace` fails as a workspace error (dogfooding D1).
fn session_workspace(cli: &Cli) -> Result<String, String> {
    Ok(radian_host::resolve_workspace(cli.workspace.as_deref())?
        .to_string_lossy()
        .into_owned())
}

/// Opens (or resumes) the session file. The caller has already resolved `session_id`.
fn open_journal(
    cli: &Cli,
    session_id: &str,
    dir: &Path,
    workspace: &str,
    provider: Provider,
) -> Result<Arc<SessionJournal>, String> {
    let path = dir.join(format!("{session_id}.jsonl"));
    if cli.resume.is_some() {
        if !path.exists() {
            return Err(format!("no session {session_id:?} in {}", dir.display()));
        }
        return SessionJournal::resume(&path, session_id)
            .map(Arc::new)
            .map_err(|e| format!("cannot resume session {session_id:?}: {e}"));
    }
    radian_session::ensure_private_dir(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // The snapshot is a model route reference only: never the key, a header value or a base
    // URL that could carry credentials (ADR 0008 §4, ADR 0013).
    let created = SessionCreatedData::new(
        workspace,
        serde_json::json!({"provider": provider.as_str(), "model": cli.model}),
    );
    match SessionJournal::create(&path, session_id, created) {
        Ok(journal) => Ok(Arc::new(journal)),
        // A complete session already occupies the file. Name the command that continues it
        // (dogfooding D15); a torn tail is still truncated by `create`.
        Err(radian_session::WriteError::NotEmpty { .. }) => Err(format!(
            "session {session_id:?} already exists in {}; continue it with `--resume \
             {session_id}`",
            dir.display()
        )),
        Err(e) => Err(format!("cannot create session {session_id:?}: {e}")),
    }
}

/// `--list`: this workspace's sessions, newest activity first. Same columns as `agent-cli`.
/// The Phase 4 startup picker: opens the workspace index, lists the sessions, and (when there
/// are any) runs the full-screen picker. `Ok(None)` means there are no sessions yet, so the
/// caller starts a new one with a notice instead of flashing an empty list.
fn pick_session(
    dir: &Path,
    workspace: &str,
) -> Result<Option<radian_tui::picker::Outcome>, String> {
    let index = radian_host::sessions::open_index(dir)?;
    let rows = match &index {
        Some(index) => radian_host::sessions::list(index, workspace)?,
        None => Vec::new(),
    };
    if rows.is_empty() {
        return Ok(None);
    }
    radian_tui::picker::run(&rows, index.as_ref(), workspace)
        .map(Some)
        .map_err(|e| format!("picker: {e}"))
}

fn list_sessions(cli: &Cli) -> Result<(), String> {
    let dir = session_dir(cli)?;
    let workspace = session_workspace(cli)?;
    if !dir.exists() {
        return Ok(()); // no sessions yet
    }
    radian_session::warn_if_wide(&dir);
    let mut index = Index::open(dir.join("index.db"))
        .map_err(|e| format!("cannot open the session index in {}: {e}", dir.display()))?;
    index
        .rebuild_dir(&dir)
        .map_err(|e| format!("cannot rebuild the session index: {e}"))?;
    let sessions = index
        .list(Some(&workspace))
        .map_err(|e| format!("cannot list sessions: {e}"))?;
    for session in sessions {
        let messages = index
            .indexed_events(&session.session_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.event == "MessageAppended")
                    .count()
            })
            .unwrap_or(0);
        println!(
            "{}\t{}\t{}\t{}",
            session.session_id,
            session.created_ts.as_deref().unwrap_or("-"),
            session.last_ts.as_deref().unwrap_or("-"),
            messages
        );
    }
    Ok(())
}

/// The built-in coding tools the TUI mounts, if `--workspace` was given. `shell` exists only
/// when a sandbox does (radian-coding refuses to advertise it otherwise); without one (the
/// `safe` tier) the file tools are still there, confined by their own workspace boundary.
fn build_coding_tools(
    workspace: Option<&Path>,
    sandbox: Option<Sandbox>,
) -> Result<Option<CodingTools>, String> {
    match (workspace, sandbox) {
        (Some(dir), Some(sandbox)) => {
            let tools = CodingTools::with_sandbox(dir, sandbox)
                .map_err(|e| format!("--workspace {}: {e}", dir.display()))?;
            Ok(Some(tools))
        }
        (Some(dir), None) => {
            let tools =
                CodingTools::new(dir).map_err(|e| format!("--workspace {}: {e}", dir.display()))?;
            Ok(Some(tools))
        }
        (None, _) => Ok(None),
    }
}

/// The host side of `ask`: forwards the request to the UI thread and waits for the answer.
/// The permission layer races this future against the turn's cancellation, so a prompt that
/// is never answered (Ctrl-C, quit) resolves as a denial instead of hanging.
struct TuiApprover {
    tx: Sender<ApprovalPrompt>,
}

impl Approver for TuiApprover {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            let (reply, answer) = tokio::sync::oneshot::channel();
            self.tx
                .send(ApprovalPrompt { request, reply })
                .map_err(|_| ApprovalError("the UI is gone".into()))?;
            answer
                .await
                .map_err(|_| ApprovalError("the approval was abandoned".into()))
        })
    }
}

/// The `--yes` approver: answers "allow once" without asking. Every decision still goes
/// through the rules first and is still written to the audit record.
struct AutoApprover;

impl Approver for AutoApprover {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async { Ok(ApprovalOutcome::AllowOnce) })
    }
}

/// Writes every decision to the session journal as a `PermissionResolved` (ADR 0006 §5.9).
/// The append is spawned: `AuditSink::record` is synchronous and runs inside the hook, so it
/// must not block. The record is audit only (`critical = false`) and is never replayed.
struct JournalAudit {
    journal: Arc<SessionJournal>,
    runtime: tokio::runtime::Handle,
}

impl AuditSink for JournalAudit {
    fn record(&self, event: &PermissionResolved) {
        let event = runtime_event(event);
        let journal = Arc::clone(&self.journal);
        self.runtime.spawn(async move {
            let _ = journal.append_runtime_event(event).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_model_call_limit_is_the_tui_one_not_the_cores() {
        let cli = Cli::try_parse_from(["radian-tui", "--model", "m"]).unwrap();
        assert_eq!(cli.max_model_calls, TUI_DEFAULT_MAX_MODEL_CALLS);
        assert_eq!(TUI_DEFAULT_MAX_MODEL_CALLS, 1000);
        let cli =
            Cli::try_parse_from(["radian-tui", "--model", "m", "--max-model-calls", "5"]).unwrap();
        assert_eq!(cli.max_model_calls, 5);
    }
}
